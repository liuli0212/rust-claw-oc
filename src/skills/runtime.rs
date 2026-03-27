//! SkillRuntime — implements `ExecutionExtension` to manage active skill lifecycle.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::core::extensions::{
    ExecutionExtension, ExtensionDecision, FinishDecision, PromptDraft, ResumeDecision,
};
use crate::tools::protocol::ToolExecutionEnvelope;
use crate::tools::Tool;

use super::definition::SkillDef;
use super::policy::SkillToolPolicy;
use super::state::{ActiveSkillState, PreambleState, SkillAnswer, SkillExecutionState};

/// The Skill Runtime — manages the lifecycle of complex skills.
///
/// Ownership model: `ActiveSkillState` is exclusively owned by this struct.
/// `AgentLoop` accesses skill state only through `ExecutionExtension` hooks.
pub struct SkillRuntime {
    /// The currently active skill state, if any.
    state: RwLock<Option<ActiveSkillState>>,
    /// The definition of the currently active skill.
    active_def: RwLock<Option<SkillDef>>,
    /// Tool policy engine.
    policy: SkillToolPolicy,
}

impl SkillRuntime {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(None),
            active_def: RwLock::new(None),
            policy: SkillToolPolicy::new(),
        }
    }

    /// Activate a skill, executing its preamble if present.
    pub async fn activate_skill(&self, def: &SkillDef) -> Result<(), String> {
        let mut state = ActiveSkillState::new(
            def.meta.name.clone(),
            def.constraints.clone(),
        );

        // Execute preamble if present
        if let Some(preamble) = &def.preamble {
            state.execution_state = SkillExecutionState::Bootstrapping;
            let result =
                super::preamble::execute_preamble(&preamble.shell, None).await;

            state.preamble_result = Some(PreambleState {
                ok: result.ok,
                vars: result.vars,
            });

            if !result.ok {
                tracing::warn!(
                    "Preamble failed for skill '{}': {}",
                    def.meta.name,
                    result.stderr
                );
                // Continue anyway — degraded mode
            }
        }

        state.execution_state = SkillExecutionState::Running;

        *self.active_def.write().await = Some(def.clone());
        *self.state.write().await = Some(state);

        tracing::info!("Skill '{}' activated", def.meta.name);
        Ok(())
    }

    /// Deactivate the current skill and clean up state.
    pub async fn deactivate_skill(&self) {
        let name = {
            let state = self.state.read().await;
            state.as_ref().map(|s| s.skill_name.clone())
        };
        *self.state.write().await = None;
        *self.active_def.write().await = None;
        if let Some(name) = name {
            tracing::info!("Skill '{}' deactivated", name);
        }
    }

    /// Whether a skill is currently active.
    pub async fn is_active(&self) -> bool {
        self.state.read().await.is_some()
    }

    /// Generate the skill contract for prompt injection.
    async fn build_contract(&self) -> Option<String> {
        let state = self.state.read().await;
        let def = self.active_def.read().await;

        let (state, def) = match (state.as_ref(), def.as_ref()) {
            (Some(s), Some(d)) => (s, d),
            _ => return None,
        };

        let mut parts = Vec::new();
        parts.push(format!(
            "## Active Skill: {} v{}",
            def.meta.name, def.meta.version
        ));
        parts.push(format!("State: {:?}", state.execution_state));

        if !def.meta.allowed_tools.is_empty() {
            parts.push(format!(
                "Allowed tools: {}",
                def.meta.allowed_tools.join(", ")
            ));
        }

        if state.constraints.forbid_code_write {
            parts.push("⚠️ HARD GATE: Do NOT write code files.".to_string());
        }

        if let Some(pi) = &state.pending_interaction {
            parts.push(format!(
                "⚠️ PENDING QUESTION (must resolve first): {}",
                pi.question
            ));
        }

        if !state.answers.is_empty() {
            parts.push(format!("Collected answers: {}", state.answers.len()));
        }

        if !state.artifacts.is_empty() {
            parts.push(format!("Produced artifacts: {}", state.artifacts.len()));
        }

        Some(parts.join("\n"))
    }
}

impl Default for SkillRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutionExtension for SkillRuntime {
    async fn before_turn_start(&self, _input: &str) -> ExtensionDecision {
        if self.is_active().await {
            ExtensionDecision::Continue
        } else {
            ExtensionDecision::Continue
        }
    }

    async fn before_prompt_build(&self, mut draft: PromptDraft) -> PromptDraft {
        if let Some(contract) = self.build_contract().await {
            draft.skill_contract = Some(contract);
        }

        let state = self.state.read().await;
        if let Some(state) = state.as_ref() {
            draft.skill_state_summary = Some(state.state_summary());
        }

        let def = self.active_def.read().await;
        if let Some(def) = def.as_ref() {
            // Truncate instructions if very long
            let instructions = if def.instructions.len() > 4000 {
                format!("{}...[truncated]", &def.instructions[..4000])
            } else {
                def.instructions.clone()
            };
            draft.skill_instructions = Some(instructions);
        }

        draft
    }

    async fn before_tool_resolution(
        &self,
        tools: Vec<Arc<dyn Tool>>,
    ) -> Vec<Arc<dyn Tool>> {
        let def = self.active_def.read().await;
        match def.as_ref() {
            Some(def) => self.policy.filter_tools(tools, def),
            None => tools,
        }
    }

    async fn after_tool_result(&self, _result: &ToolExecutionEnvelope) {
        // Future: update state based on tool results
    }

    async fn on_user_resume(&self, input: &str) -> ResumeDecision {
        let mut state = self.state.write().await;
        let state = match state.as_mut() {
            Some(s) => s,
            None => return ResumeDecision::PassThrough,
        };

        if let Some(pi) = state.pending_interaction.take() {
            let answer = SkillAnswer {
                question: pi.question,
                answer: input.to_string(),
                answered_at: chrono::Utc::now().to_rfc3339(),
            };
            state.answers.insert(pi.context_key.clone(), answer);
            state.execution_state = SkillExecutionState::Running;

            return ResumeDecision::ResumeSkill {
                context_key: pi.context_key,
                answer: input.to_string(),
            };
        }

        ResumeDecision::PassThrough
    }

    async fn before_finish(&self) -> FinishDecision {
        let state = self.state.read().await;
        let state = match state.as_ref() {
            Some(s) => s,
            None => return FinishDecision::Allow,
        };

        // Check artifact contract
        if let Some(required_kind) = &state.constraints.required_artifact_kind {
            if state.artifacts.is_empty() {
                return FinishDecision::Deny {
                    reason: format!(
                        "Skill '{}' requires a {:?} artifact before completion.",
                        state.skill_name, required_kind
                    ),
                };
            }
        }

        FinishDecision::Allow
    }
}
