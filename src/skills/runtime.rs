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
    pub async fn activate_skill(
        &self,
        def: &SkillDef,
        initial_args: Option<String>,
    ) -> Result<(), String> {
        let mut state = ActiveSkillState::new(
            def.meta.name.clone(),
            def.constraints.clone(),
        );
        state.initial_args = initial_args;

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
        let state = self.state.read().await;

        let mut filtered = match def.as_ref() {
            Some(def) => self.policy.filter_tools(tools, def),
            None => return tools,
        };

        // Enforce forbid_code_write hard gate
        if let Some(state) = state.as_ref() {
            if state.constraints.forbid_code_write {
                let write_tools = ["write_file", "patch_file"];
                filtered.retain(|t| !write_tools.contains(&t.name().as_str()));
            }
        }

        filtered
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::{ExecutionExtension, FinishDecision, PromptDraft, ResumeDecision};
    use crate::skills::definition::*;
    use crate::skills::state::PendingInteraction;

    fn make_test_skill(forbid_code: bool, required_artifact: Option<ArtifactKind>) -> SkillDef {
        SkillDef {
            meta: SkillMeta {
                name: "test_skill".to_string(),
                version: "1.0".to_string(),
                description: "Test skill".to_string(),
                trigger: SkillTrigger::ManualOnly,
                allowed_tools: vec!["read_file".to_string(), "execute_bash".to_string()],
                output_mode: None,
            },
            instructions: "Do the thing.".to_string(),
            preamble: None,
            constraints: SkillConstraints {
                forbid_code_write: forbid_code,
                allow_subagents: false,
                require_question_resume: false,
                required_artifact_kind: required_artifact,
            },
        }
    }

    #[tokio::test]
    async fn test_activate_deactivate_lifecycle() {
        let rt = SkillRuntime::new();
        assert!(!rt.is_active().await);

        let skill = make_test_skill(false, None);
        rt.activate_skill(&skill, None).await.unwrap();
        assert!(rt.is_active().await);

        rt.deactivate_skill().await;
        assert!(!rt.is_active().await);
    }

    #[tokio::test]
    async fn test_activate_with_args_injects_into_prompt() {
        let rt = SkillRuntime::new();
        let skill = make_test_skill(false, None);
        rt.activate_skill(&skill, Some("a space cat".to_string()))
            .await
            .unwrap();

        let draft = rt.before_prompt_build(PromptDraft::default()).await;
        let summary = draft.skill_state_summary.unwrap();
        assert!(summary.contains("USER INPUT AT ACTIVATION: a space cat"));
    }

    #[tokio::test]
    async fn test_prompt_build_injects_contract() {
        let rt = SkillRuntime::new();
        let skill = make_test_skill(true, None);
        rt.activate_skill(&skill, None).await.unwrap();

        let draft = rt.before_prompt_build(PromptDraft::default()).await;
        assert!(draft.skill_contract.is_some());
        let contract = draft.skill_contract.unwrap();
        assert!(contract.contains("test_skill"));
        assert!(contract.contains("HARD GATE"));
    }

    #[tokio::test]
    async fn test_prompt_build_without_skill_is_noop() {
        let rt = SkillRuntime::new();
        let draft = rt.before_prompt_build(PromptDraft::default()).await;
        assert!(draft.skill_contract.is_none());
        assert!(draft.skill_instructions.is_none());
    }

    #[tokio::test]
    async fn test_forbid_code_write_filters_tools() {
        let rt = SkillRuntime::new();
        let skill = make_test_skill(true, None);
        rt.activate_skill(&skill, None).await.unwrap();

        // Create mock tools
        struct MockTool(String);
        #[async_trait]
        impl Tool for MockTool {
            fn name(&self) -> String { self.0.clone() }
            fn description(&self) -> String { String::new() }
            fn parameters_schema(&self) -> serde_json::Value { serde_json::json!({}) }
            async fn execute(&self, _: serde_json::Value, _: &crate::tools::protocol::ToolContext) -> Result<String, crate::tools::protocol::ToolError> { Ok(String::new()) }
        }

        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(MockTool("read_file".to_string())),
            Arc::new(MockTool("write_file".to_string())),
            Arc::new(MockTool("patch_file".to_string())),
            Arc::new(MockTool("execute_bash".to_string())),
            Arc::new(MockTool("finish_task".to_string())),
        ];

        let filtered = rt.before_tool_resolution(tools).await;
        let names: Vec<String> = filtered.iter().map(|t| t.name()).collect();

        // write_file and patch_file should be removed by forbid_code_write
        assert!(!names.contains(&"write_file".to_string()));
        assert!(!names.contains(&"patch_file".to_string()));
        // Allowed tools should remain
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"execute_bash".to_string()));
        // Runtime tools always allowed
        assert!(names.contains(&"finish_task".to_string()));
    }

    #[tokio::test]
    async fn test_resume_with_pending_interaction() {
        let rt = SkillRuntime::new();
        let skill = make_test_skill(false, None);
        rt.activate_skill(&skill, None).await.unwrap();

        // Set pending interaction
        {
            let mut state = rt.state.write().await;
            let state = state.as_mut().unwrap();
            state.pending_interaction = Some(PendingInteraction {
                skill_name: "test_skill".to_string(),
                context_key: "project_name".to_string(),
                question: "What is the project name?".to_string(),
                options: vec![],
                recommendation: None,
                asked_at: "now".to_string(),
            });
            state.execution_state = SkillExecutionState::WaitingUser;
        }

        let decision = rt.on_user_resume("MyProject").await;
        match decision {
            ResumeDecision::ResumeSkill { context_key, answer } => {
                assert_eq!(context_key, "project_name");
                assert_eq!(answer, "MyProject");
            }
            _ => panic!("Expected ResumeSkill"),
        }

        // Verify answer was stored
        let state = rt.state.read().await;
        let state = state.as_ref().unwrap();
        assert!(state.answers.contains_key("project_name"));
        assert_eq!(state.execution_state, SkillExecutionState::Running);
    }

    #[tokio::test]
    async fn test_resume_without_pending_passes_through() {
        let rt = SkillRuntime::new();
        let skill = make_test_skill(false, None);
        rt.activate_skill(&skill, None).await.unwrap();

        let decision = rt.on_user_resume("hello").await;
        assert!(matches!(decision, ResumeDecision::PassThrough));
    }

    #[tokio::test]
    async fn test_artifact_contract_denies_finish() {
        let rt = SkillRuntime::new();
        let skill = make_test_skill(false, Some(ArtifactKind::DesignDoc));
        rt.activate_skill(&skill, None).await.unwrap();

        let decision = rt.before_finish().await;
        match decision {
            FinishDecision::Deny { reason } => {
                assert!(reason.contains("DesignDoc"));
            }
            _ => panic!("Expected Deny"),
        }
    }

    #[tokio::test]
    async fn test_artifact_contract_allows_finish_with_artifact() {
        let rt = SkillRuntime::new();
        let skill = make_test_skill(false, Some(ArtifactKind::DesignDoc));
        rt.activate_skill(&skill, None).await.unwrap();

        // Add an artifact
        {
            let mut state = rt.state.write().await;
            let state = state.as_mut().unwrap();
            state.artifacts.push(crate::skills::state::SkillArtifact {
                kind: "design_doc".to_string(),
                path: "/tmp/design.md".to_string(),
                summary: Some("Design doc".to_string()),
            });
        }

        let decision = rt.before_finish().await;
        assert!(matches!(decision, FinishDecision::Allow));
    }

    #[tokio::test]
    async fn test_no_skill_allows_finish() {
        let rt = SkillRuntime::new();
        let decision = rt.before_finish().await;
        assert!(matches!(decision, FinishDecision::Allow));
    }
}
