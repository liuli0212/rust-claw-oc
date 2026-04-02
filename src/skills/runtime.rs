//! SkillRuntime — implements `ExecutionExtension` to manage active skill lifecycle.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::core::extensions::{ExecutionExtension, ExtensionDecision, FinishDecision, PromptDraft};
use crate::tools::protocol::ToolExecutionEnvelope;
use crate::tools::Tool;

use super::call_tree::{SkillCallContext, SkillSessionSeed};
use super::definition::SkillDef;
use super::policy::SkillToolPolicy;
use super::registry::SkillRegistry;
use super::state::{ActiveSkillState, SkillAnswer, SkillExecutionState};

/// The Skill Runtime — manages the lifecycle of complex skills.
///
/// Ownership model: `ActiveSkillState` is exclusively owned by this struct.
/// `AgentLoop` accesses skill state only through `ExecutionExtension` hooks.
pub struct SkillRuntime {
    session_id: String,
    /// The currently active skill state, if any.
    state: RwLock<Option<ActiveSkillState>>,
    /// The definition of the currently active skill.
    active_def: RwLock<Option<SkillDef>>,
    active_call_context: RwLock<Option<SkillCallContext>>,
    /// Tool policy engine.
    policy: SkillToolPolicy,
    registry: SkillRegistry,
    session_seed: SkillSessionSeed,
}

impl SkillRuntime {
    pub fn new() -> Self {
        Self::new_for_session("standalone")
    }

    pub fn new_for_session(session_id: impl Into<String>) -> Self {
        tracing::debug!("Initializing SkillRuntime and discovering skills...");
        let mut registry = SkillRegistry::new();
        registry.discover(Path::new("skills"));
        Self::with_registry_for_session(session_id, registry)
    }

    pub fn with_registry(registry: SkillRegistry) -> Self {
        Self::with_registry_for_session("standalone", registry)
    }

    pub fn with_registry_for_session(
        session_id: impl Into<String>,
        registry: SkillRegistry,
    ) -> Self {
        Self::with_registry_and_seed(session_id, registry, SkillSessionSeed::default())
    }

    pub fn with_session_seed(
        session_id: impl Into<String>,
        session_seed: SkillSessionSeed,
    ) -> Self {
        tracing::debug!("Initializing SkillRuntime and discovering skills...");
        let mut registry = SkillRegistry::new();
        registry.discover(Path::new("skills"));
        Self::with_registry_and_seed(session_id, registry, session_seed)
    }

    pub fn with_registry_and_seed(
        session_id: impl Into<String>,
        registry: SkillRegistry,
        session_seed: SkillSessionSeed,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            state: RwLock::new(None),
            active_def: RwLock::new(None),
            active_call_context: RwLock::new(None),
            policy: SkillToolPolicy::new(),
            registry,
            session_seed,
        }
    }

    fn truncate_for_prompt(input: &str, max_chars: usize) -> String {
        let mut chars = input.chars();
        let truncated: String = chars.by_ref().take(max_chars).collect();
        if chars.next().is_some() {
            format!("{truncated}...[truncated]")
        } else {
            truncated
        }
    }

    fn derive_call_context(
        &self,
        skill_name: &str,
        initial_args: Option<&str>,
    ) -> SkillCallContext {
        self.session_seed
            .inherited_call_context
            .clone()
            .unwrap_or_else(|| SkillCallContext::new_root(self.session_id.clone()))
            .append_frame(skill_name, initial_args)
    }

    /// Activate a skill for the current session.
    pub async fn activate_skill(
        &self,
        def: &SkillDef,
        initial_args: Option<String>,
    ) -> Result<(), String> {
        tracing::info!(
            "Activating skill '{}' with args: {:?}",
            def.meta.name,
            initial_args
        );
        let call_context = self.derive_call_context(&def.meta.name, initial_args.as_deref());
        let lineage_names = call_context.lineage_names();
        let mut state = ActiveSkillState::new(def.meta.name.clone(), def.constraints.clone());
        state.initial_args = initial_args;
        state.execution_state = SkillExecutionState::Running;
        state
            .labels
            .insert("phase".to_string(), "running".to_string());
        if let Some(output_mode) = def.meta.output_mode.as_ref() {
            state.labels.insert(
                "output_mode".to_string(),
                Self::output_mode_name(output_mode).to_string(),
            );
        }
        state.labels.insert(
            "call_depth".to_string(),
            call_context.current_depth().to_string(),
        );
        state
            .labels
            .insert("call_lineage".to_string(), lineage_names.join(" -> "));
        state.labels.insert(
            "root_session_id".to_string(),
            call_context.root_session_id.clone(),
        );
        if let Some(frame) = call_context.lineage.last() {
            state
                .labels
                .insert("call_id".to_string(), frame.call_id.clone());
            if let Some(parent_call_id) = frame.parent_call_id.as_ref() {
                state
                    .labels
                    .insert("parent_call_id".to_string(), parent_call_id.clone());
            }
            if let Some(args_digest) = frame.args_digest.as_ref() {
                state
                    .labels
                    .insert("args_digest".to_string(), args_digest.clone());
            }
        }

        *self.active_def.write().await = Some(def.clone());
        *self.active_call_context.write().await = Some(call_context);
        *self.state.write().await = Some(state);

        tracing::info!("Skill '{}' activated", def.meta.name);
        Ok(())
    }

    /// Deactivate the current skill and clean up state.
    #[allow(dead_code)]
    pub async fn deactivate_skill(&self) {
        let name = {
            let state = self.state.read().await;
            state.as_ref().map(|s| s.skill_name.clone())
        };
        *self.state.write().await = None;
        *self.active_def.write().await = None;
        *self.active_call_context.write().await = None;
        if let Some(name) = name {
            tracing::info!("Skill '{}' deactivated", name);
        }
    }

    /// Whether a skill is currently active.
    #[allow(dead_code)]
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
        parts.push(format!(
            "Trigger: {}",
            Self::trigger_name(&def.meta.trigger)
        ));

        if let Some(output_mode) = def.meta.output_mode.as_ref() {
            parts.push(format!(
                "Output mode: {}",
                Self::output_mode_name(output_mode)
            ));
        }

        if !def.meta.allowed_tools.is_empty() {
            parts.push(format!(
                "Allowed tools: {}",
                def.meta.allowed_tools.join(", ")
            ));
        }

        if state.constraints.forbid_code_write {
            parts.push("⚠️ HARD GATE: Do NOT write code files.".to_string());
        }

        if let Some(required_artifact_kind) =
            Self::effective_required_artifact_kind(state, Some(def))
        {
            parts.push(format!(
                "Required artifact: {}",
                Self::required_artifact_kind_name(&required_artifact_kind)
            ));
        }

        Some(parts.join("\n"))
    }

    async fn activate_skill_from_command(&self, input: &str) -> Result<Option<String>, String> {
        let trimmed = input.trim();
        if !trimmed.starts_with('/') {
            return Ok(None);
        }

        let mut parts = trimmed.splitn(2, ' ');
        let cmd = parts.next().unwrap();
        let skill_name = &cmd[1..];

        if skill_name.is_empty() {
            return Ok(None);
        }

        let activation_args = parts
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string);

        let Some(def) = self.registry.clone_skill(skill_name) else {
            let available = self.registry.names().join(", ");
            if skill_name == "skill" {
                return Err(if available.is_empty() {
                    "Usage: /<skill_name> [activation args]".to_string()
                } else {
                    format!(
                        "Usage: /<skill_name> [activation args]\nAvailable skills: {}",
                        available
                    )
                });
            }
            return Ok(None);
        };

        if matches!(
            def.meta.trigger,
            super::definition::SkillTrigger::SuggestOnly
        ) {
            return Err(format!(
                "Skill '{}' is suggest_only and cannot be activated manually.",
                def.meta.name
            ));
        }

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

    fn required_artifact_kind_name(
        required_kind: &super::definition::ArtifactKind,
    ) -> &'static str {
        match required_kind {
            super::definition::ArtifactKind::DesignDoc => "design_doc",
            super::definition::ArtifactKind::ReviewReport => "review_report",
        }
    }

    fn trigger_name(trigger: &super::definition::SkillTrigger) -> &'static str {
        match trigger {
            super::definition::SkillTrigger::ManualOnly => "manual_only",
            super::definition::SkillTrigger::SuggestOnly => "suggest_only",
            super::definition::SkillTrigger::ManualOrSuggested => "manual_or_suggested",
        }
    }

    fn output_mode_name(output_mode: &super::definition::OutputMode) -> &'static str {
        match output_mode {
            super::definition::OutputMode::Freeform => "freeform",
            super::definition::OutputMode::DesignDocOnly => "design_doc_only",
            super::definition::OutputMode::ReviewOnly => "review_only",
        }
    }

    fn artifact_kind_for_output_mode(
        output_mode: &super::definition::OutputMode,
    ) -> Option<super::definition::ArtifactKind> {
        match output_mode {
            super::definition::OutputMode::Freeform => None,
            super::definition::OutputMode::DesignDocOnly => {
                Some(super::definition::ArtifactKind::DesignDoc)
            }
            super::definition::OutputMode::ReviewOnly => {
                Some(super::definition::ArtifactKind::ReviewReport)
            }
        }
    }

    fn effective_required_artifact_kind(
        state: &ActiveSkillState,
        def: Option<&SkillDef>,
    ) -> Option<super::definition::ArtifactKind> {
        state
            .constraints
            .required_artifact_kind
            .clone()
            .or_else(|| {
                def.and_then(|definition| {
                    definition
                        .meta
                        .output_mode
                        .as_ref()
                        .and_then(Self::artifact_kind_for_output_mode)
                })
            })
    }

    fn is_terminal_subagent_status(status: &str) -> bool {
        matches!(status, "finished" | "failed" | "cancelled" | "timed_out")
    }

    async fn consume_pending_interaction(&self, input: &str) -> bool {
        let resumed = {
            let mut state_guard = self.state.write().await;
            let Some(state) = state_guard.as_mut() else {
                return false;
            };

            state.pending_interaction.take().map(|pi| {
                let skill_name = state.skill_name.clone();
                let context_key = pi.context_key.clone();
                let answer = SkillAnswer {
                    question: pi.question,
                    answer: input.to_string(),
                    answered_at: chrono::Utc::now().to_rfc3339(),
                };
                state.answers.insert(context_key.clone(), answer);
                state.execution_state = SkillExecutionState::Running;
                state
                    .labels
                    .insert("phase".to_string(), "running".to_string());
                state.labels.remove("pending_context_key");
                (skill_name, context_key)
            })
        };

        if let Some((skill_name, context_key)) = resumed {
            tracing::info!(
                skill = %skill_name,
                context_key = %context_key,
                "Skill resumed from WaitingUser to Running"
            );
            return true;
        }

        false
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
        if self.consume_pending_interaction(input).await {
            return ExtensionDecision::Continue;
        }

        match self.activate_skill_from_command(input).await {
            Ok(Some(overlay)) => {
                return ExtensionDecision::Intercept {
                    prompt_overlay: Some(overlay),
                };
            }
            Ok(None) => {}
            Err(message) => return ExtensionDecision::Halt { message },
        }

        let state = self.state.read().await;
        if let Some(state) = state.as_ref() {
            if state.constraints.forbid_code_write && input.contains("```") {
                // This is a soft nudge, but we could make it a hard gate if needed.
                // For now we rely on tool filtering.
            }
        }

        ExtensionDecision::Continue
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
            let instructions = Self::truncate_for_prompt(&def.instructions, 4000);
            draft.skill_instructions = Some(instructions);
        }

        draft
    }

    async fn before_tool_resolution(&self, tools: Vec<Arc<dyn Tool>>) -> Vec<Arc<dyn Tool>> {
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

    async fn enrich_tool_context(
        &self,
        mut ctx: crate::tools::ToolContext,
    ) -> crate::tools::ToolContext {
        if let Some(state) = self.state.read().await.as_ref() {
            ctx.active_skill_name = Some(state.skill_name.clone());
        }
        if let Some(call_context) = self.active_call_context.read().await.as_ref() {
            ctx.skill_call_context = Some(call_context.clone());
        }
        ctx
    }

    async fn after_tool_result(&self, result: &ToolExecutionEnvelope) {
        let active_def = self.active_def.read().await.clone();
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
            state
                .labels
                .insert("phase".to_string(), "waiting_user".to_string());
            state.labels.insert(
                "pending_context_key".to_string(),
                request.context_key.clone(),
            );
        }

        if let Some(path) = &result.effects.file_path {
            let kind = Self::effective_required_artifact_kind(state, active_def.as_ref())
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

        if result.result.ok {
            let payload = serde_json::from_str::<serde_json::Value>(&result.result.output).ok();
            match result.result.tool_name.as_str() {
                "spawn_subagent" => {
                    state.execution_state = SkillExecutionState::WaitingSubagent;
                    state
                        .labels
                        .insert("phase".to_string(), "waiting_subagent".to_string());
                    if let Some(job_id) = payload
                        .as_ref()
                        .and_then(|value| value.get("job_id"))
                        .and_then(|value| value.as_str())
                    {
                        state
                            .labels
                            .insert("waiting_on_subagent_job_id".to_string(), job_id.to_string());
                    }
                }
                "get_subagent_result" => {
                    let status = payload
                        .as_ref()
                        .and_then(|value| value.get("status"))
                        .and_then(|value| value.as_str())
                        .unwrap_or_default();
                    if Self::is_terminal_subagent_status(status) {
                        state.execution_state = SkillExecutionState::Running;
                        state
                            .labels
                            .insert("phase".to_string(), "running".to_string());
                        state.labels.remove("waiting_on_subagent_job_id");
                    } else if !status.is_empty() {
                        state.execution_state = SkillExecutionState::WaitingSubagent;
                        state
                            .labels
                            .insert("phase".to_string(), "waiting_subagent".to_string());
                    }
                }
                "cancel_subagent" => {
                    state.execution_state = SkillExecutionState::Running;
                    state
                        .labels
                        .insert("phase".to_string(), "running".to_string());
                    state.labels.remove("waiting_on_subagent_job_id");
                }
                _ => {}
            }
        }
    }

    async fn before_finish(&self) -> FinishDecision {
        let active_def = self.active_def.read().await.clone();
        let mut state = self.state.write().await;
        let state = match state.as_mut() {
            Some(s) => s,
            None => return FinishDecision::Allow,
        };

        state.execution_state = SkillExecutionState::ValidatingArtifacts;
        state
            .labels
            .insert("phase".to_string(), "validating_artifacts".to_string());

        // Check artifact contract
        if let Some(required_kind) =
            Self::effective_required_artifact_kind(state, active_def.as_ref())
        {
            let required_name = Self::required_artifact_kind_name(&required_kind);
            let has_required_artifact = state
                .artifacts
                .iter()
                .any(|artifact| artifact.kind == required_name);
            if !has_required_artifact {
                state.execution_state = SkillExecutionState::Running;
                state
                    .labels
                    .insert("phase".to_string(), "running".to_string());
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

    async fn on_finish_committed(&self, _summary: &str) {
        let skill_name = {
            let mut state = self.state.write().await;
            let Some(state) = state.as_mut() else {
                return;
            };
            state.execution_state = SkillExecutionState::Completed;
            state
                .labels
                .insert("phase".to_string(), "completed".to_string());
            state.skill_name.clone()
        };

        tracing::info!(skill = %skill_name, "Skill finished; cleaning up active state");
        self.deactivate_skill().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::{ExecutionExtension, FinishDecision, PromptDraft};
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
            parameters: None,
            constraints: SkillConstraints {
                forbid_code_write: forbid_code,

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
    async fn test_prompt_build_injects_output_mode_into_contract() {
        let rt = SkillRuntime::new();
        let mut skill = make_test_skill(false, None);
        skill.meta.output_mode = Some(OutputMode::ReviewOnly);
        rt.activate_skill(&skill, None).await.unwrap();

        let draft = rt.before_prompt_build(PromptDraft::default()).await;
        let contract = draft.skill_contract.unwrap();
        assert!(contract.contains("Output mode: review_only"));
        assert!(contract.contains("Trigger: manual_only"));
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
    async fn test_prompt_build_separates_contract_from_runtime_state() {
        let rt = SkillRuntime::new();
        let skill = make_test_skill(false, None);
        rt.activate_skill(&skill, Some("review this".to_string()))
            .await
            .unwrap();

        let draft = rt.before_prompt_build(PromptDraft::default()).await;
        let contract = draft.skill_contract.unwrap();
        let summary = draft.skill_state_summary.unwrap();

        assert!(contract.contains("Trigger: manual_only"));
        assert!(!contract.contains("State:"));
        assert!(!contract.contains("USER INPUT AT ACTIVATION"));
        assert!(summary.contains("State: Running"));
        assert!(summary.contains("USER INPUT AT ACTIVATION: review this"));
    }

    #[tokio::test]
    async fn test_prompt_build_without_skill_is_noop() {
        let rt = SkillRuntime::new();
        let draft = rt.before_prompt_build(PromptDraft::default()).await;
        assert!(draft.skill_contract.is_none());
        assert!(draft.skill_instructions.is_none());
    }

    #[tokio::test]
    async fn test_prompt_build_truncates_utf8_instructions_safely() {
        let rt = SkillRuntime::new();
        let mut skill = make_test_skill(false, None);
        skill.instructions = "你".repeat(4001);
        rt.activate_skill(&skill, None).await.unwrap();

        let draft = rt.before_prompt_build(PromptDraft::default()).await;
        let instructions = draft.skill_instructions.unwrap();

        assert!(instructions.ends_with("...[truncated]"));
        assert!(instructions.starts_with('你'));
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
            fn name(&self) -> String {
                self.0.clone()
            }
            fn description(&self) -> String {
                String::new()
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(
                &self,
                _: serde_json::Value,
                _: &crate::tools::protocol::ToolContext,
            ) -> Result<String, crate::tools::protocol::ToolError> {
                Ok(String::new())
            }
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

        assert!(matches!(
            rt.before_turn_start("MyProject").await,
            ExtensionDecision::Continue
        ));

        // Verify answer was stored
        let state = rt.state.read().await;
        let state = state.as_ref().unwrap();
        assert!(state.answers.contains_key("project_name"));
        assert_eq!(state.execution_state, SkillExecutionState::Running);
        assert!(state.pending_interaction.is_none());
    }

    #[tokio::test]
    async fn test_resume_without_pending_passes_through() {
        let rt = SkillRuntime::new();
        let skill = make_test_skill(false, None);
        rt.activate_skill(&skill, None).await.unwrap();

        assert!(matches!(
            rt.before_turn_start("hello").await,
            ExtensionDecision::Continue
        ));

        let state = rt.state.read().await;
        let state = state.as_ref().unwrap();
        assert!(state.answers.is_empty());
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
    async fn test_output_mode_review_only_denies_finish_without_artifact() {
        let rt = SkillRuntime::new();
        let mut skill = make_test_skill(false, None);
        skill.meta.output_mode = Some(OutputMode::ReviewOnly);
        rt.activate_skill(&skill, None).await.unwrap();

        let decision = rt.before_finish().await;
        assert!(matches!(decision, FinishDecision::Deny { .. }));
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

        let decision = rt
            .before_turn_start("/test_skill collect requirements")
            .await;
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
    async fn test_before_turn_start_ignores_unknown_slash_input() {
        let rt = SkillRuntime::with_registry(SkillRegistry::new());
        let decision = rt.before_turn_start("/tmp/a.txt").await;
        assert!(matches!(decision, ExtensionDecision::Continue));
        assert!(!rt.is_active().await);
    }

    #[tokio::test]
    async fn test_before_turn_start_rejects_manual_activation_of_suggest_only_skill() {
        let mut registry = SkillRegistry::new();
        let mut skill = make_test_skill(false, None);
        skill.meta.name = "suggested_only".to_string();
        skill.meta.trigger = SkillTrigger::SuggestOnly;
        registry.insert(skill);
        let rt = SkillRuntime::with_registry(registry);

        let decision = rt.before_turn_start("/suggested_only").await;
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
        })
        .await;

        let state = rt.state.read().await;
        let state = state.as_ref().unwrap();
        assert!(matches!(
            state.execution_state,
            SkillExecutionState::WaitingUser
        ));
        assert_eq!(
            state
                .pending_interaction
                .as_ref()
                .map(|pi| pi.context_key.as_str()),
            Some("goal")
        );
    }

    #[tokio::test]
    async fn test_after_tool_result_tracks_spawned_subagent_state() {
        let rt = SkillRuntime::new();
        let skill = make_test_skill(false, None);
        rt.activate_skill(&skill, None).await.unwrap();

        rt.after_tool_result(&ToolExecutionEnvelope {
            result: crate::tools::protocol::ToolResultData {
                ok: true,
                tool_name: "spawn_subagent".to_string(),
                output: serde_json::json!({
                    "job_id": "job-123",
                    "status": "spawned"
                })
                .to_string(),
                exit_code: None,
                duration_ms: None,
                truncated: false,
            },
            effects: crate::tools::protocol::ToolEffects::default(),
        })
        .await;

        let state = rt.state.read().await;
        let state = state.as_ref().unwrap();
        assert_eq!(state.execution_state, SkillExecutionState::WaitingSubagent);
        assert_eq!(
            state
                .labels
                .get("waiting_on_subagent_job_id")
                .map(String::as_str),
            Some("job-123")
        );
    }

    #[tokio::test]
    async fn test_on_finish_committed_deactivates_skill() {
        let rt = SkillRuntime::new();
        let skill = make_test_skill(false, None);
        rt.activate_skill(&skill, None).await.unwrap();

        rt.on_finish_committed("done").await;
        assert!(!rt.is_active().await);
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

        assert!(matches!(
            rt.before_finish().await,
            FinishDecision::Deny { .. }
        ));
    }
}
