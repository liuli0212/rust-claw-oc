//! SkillRuntime — implements `ExecutionExtension` to manage active skill lifecycle.

use std::path::Path;
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
use super::registry::SkillRegistry;
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
    registry: SkillRegistry,
}

impl SkillRuntime {
    pub fn new() -> Self {
        let mut registry = SkillRegistry::new();
        registry.discover(Path::new("skills"));
        Self::with_registry(registry)
    }

    pub fn with_registry(registry: SkillRegistry) -> Self {
        Self {
            state: RwLock::new(None),
            active_def: RwLock::new(None),
            policy: SkillToolPolicy::new(),
            registry,
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

    async fn activate_skill_from_command(
        &self,
        input: &str,
    ) -> Result<Option<String>, String> {
        let trimmed = input.trim();
        if !trimmed.starts_with("/skill") {
            return Ok(None);
        }

        let mut parts = trimmed.splitn(3, ' ');
        let _cmd = parts.next();
        let skill_name = match parts.next() {
            Some(name) if !name.trim().is_empty() => name.trim(),
            _ => {
                let available = self.registry.names().join(", ");
                return Err(if available.is_empty() {
                    "Usage: /skill <name> [activation args]".to_string()
                } else {
                    format!("Usage: /skill <name> [activation args]\nAvailable skills: {}", available)
                });
            }
        };
        let activation_args = parts
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string);

        let Some(def) = self.registry.clone_skill(skill_name) else {
            let available = self.registry.names().join(", ");
            return Err(if available.is_empty() {
                format!("Unknown skill '{}'.", skill_name)
            } else {
                format!("Unknown skill '{}'. Available skills: {}", skill_name, available)
            });
        };

        self.activate_skill(&def, activation_args.clone()).await?;

        let mut message = format!(
            "Activated skill '{}'. Follow the active skill contract for this turn.",
            def.meta.name
        );
        if let Some(args) = activation_args {
            message.push_str(&format!("\nActivation args: {}", args));
        }
        Ok(Some(message))
    }

    fn required_artifact_kind_name(required_kind: &super::definition::ArtifactKind) -> &'static str {
        match required_kind {
            super::definition::ArtifactKind::DesignDoc => "design_doc",
            super::definition::ArtifactKind::ReviewReport => "review_report",
        }
    }
}

impl Default for SkillRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutionExtension for SkillRuntime {
    async fn before_turn_start(&self, input: &str) -> ExtensionDecision {
        match self.activate_skill_from_command(input).await {
            Ok(Some(overlay)) => ExtensionDecision::Intercept {
                prompt_overlay: Some(overlay),
            },
            Ok(None) => ExtensionDecision::Continue,
            Err(message) => ExtensionDecision::Halt { message },
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

    async fn after_tool_result(&self, result: &ToolExecutionEnvelope) {
        let mut state = self.state.write().await;
        let Some(state) = state.as_mut() else {
            return;
        };

        if let Some(request) = &result.effects.await_user {
            state.pending_interaction = Some(super::state::PendingInteraction {
                skill_name: state.skill_name.clone(),
                context_key: request.context_key.clone(),
                question: request.question.clone(),
                options: request.options.clone(),
                recommendation: request.recommendation.clone(),
                asked_at: chrono::Utc::now().to_rfc3339(),
            });
            state.execution_state = SkillExecutionState::WaitingUser;
        }

        if let Some(path) = &result.effects.file_path {
            let kind = state
                .constraints
                .required_artifact_kind
                .as_ref()
                .map(Self::required_artifact_kind_name)
                .unwrap_or("file")
                .to_string();
            state.artifacts.push(super::state::SkillArtifact {
                kind,
                path: path.clone(),
                summary: Some(format!("Produced by {}", result.result.tool_name)),
            });
        }
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
            let required_name = Self::required_artifact_kind_name(required_kind);
            let has_required_artifact = state
                .artifacts
                .iter()
                .any(|artifact| artifact.kind == required_name);
            if !has_required_artifact {
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
                parameters: None,
            },
            instructions: "Do the thing.".to_string(),
            preamble: None,
            parameters: None,
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

    #[tokio::test]
    async fn test_before_turn_start_activates_skill_from_command() {
        let mut registry = SkillRegistry::new();
        registry.insert(make_test_skill(false, None));
        let rt = SkillRuntime::with_registry(registry);

        let decision = rt.before_turn_start("/skill test_skill collect requirements").await;
        match decision {
            ExtensionDecision::Intercept { prompt_overlay } => {
                let overlay = prompt_overlay.expect("expected overlay");
                assert!(overlay.contains("Activated skill 'test_skill'"));
                assert!(rt.is_active().await);
            }
            other => panic!("expected intercept, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_before_turn_start_halts_on_unknown_skill() {
        let rt = SkillRuntime::with_registry(SkillRegistry::new());
        let decision = rt.before_turn_start("/skill missing").await;
        assert!(matches!(decision, ExtensionDecision::Halt { .. }));
    }

    #[tokio::test]
    async fn test_after_tool_result_sets_pending_interaction() {
        let rt = SkillRuntime::new();
        let skill = make_test_skill(false, None);
        rt.activate_skill(&skill, None).await.unwrap();

        rt.after_tool_result(&ToolExecutionEnvelope {
            result: crate::tools::protocol::ToolResultData {
                ok: true,
                tool_name: "ask_user_question".to_string(),
                output: "waiting".to_string(),
                exit_code: None,
                duration_ms: None,
                truncated: false,
            },
            effects: crate::tools::protocol::ToolEffects {
                await_user: Some(crate::tools::protocol::UserPromptRequest {
                    question: "What is your goal?".to_string(),
                    context_key: "goal".to_string(),
                    options: vec!["a".to_string(), "b".to_string()],
                    recommendation: Some("a".to_string()),
                }),
                ..Default::default()
            },
        }).await;

        let state = rt.state.read().await;
        let state = state.as_ref().unwrap();
        assert!(matches!(state.execution_state, SkillExecutionState::WaitingUser));
        assert_eq!(
            state.pending_interaction.as_ref().map(|pi| pi.context_key.as_str()),
            Some("goal")
        );
    }

    #[tokio::test]
    async fn test_artifact_contract_denies_finish_with_wrong_artifact_kind() {
        let rt = SkillRuntime::new();
        let skill = make_test_skill(false, Some(ArtifactKind::DesignDoc));
        rt.activate_skill(&skill, None).await.unwrap();

        {
            let mut state = rt.state.write().await;
            let state = state.as_mut().unwrap();
            state.artifacts.push(crate::skills::state::SkillArtifact {
                kind: "review_report".to_string(),
                path: "/tmp/review.md".to_string(),
                summary: None,
            });
        }

        assert!(matches!(rt.before_finish().await, FinishDecision::Deny { .. }));
    }
}
