//! Minimal skill runtime for top-level skill invocation and interactive resume.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::call_chain::{CallChainContext, CallChainSeed, MAX_CALL_CHAIN_DEPTH};
use crate::core::extensions::{ExecutionExtension, ExtensionDecision, FinishDecision, PromptDraft};
use crate::tools::protocol::ToolExecutionEnvelope;
use crate::tools::Tool;

use super::arguments::{
    format_prompt_argument_sections, parse_invocation_args, validate_json_args,
};
use super::definition::{SkillDef, SkillTrigger};
use super::policy::SkillToolPolicy;
use super::registry::SkillRegistry;
use super::state::{PendingInteraction, SkillInvocation, SkillInvocationState};

pub struct SkillRuntime {
    session_id: String,
    invocation: RwLock<Option<SkillInvocation>>,
    policy: SkillToolPolicy,
    registry: SkillRegistry,
    call_chain_seed: CallChainSeed,
}

impl SkillRuntime {
    pub fn new() -> Self {
        Self::new_for_session("standalone")
    }

    pub fn new_for_session(session_id: impl Into<String>) -> Self {
        tracing::debug!("Initializing minimal SkillRuntime and discovering skills...");
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
        Self::with_registry_and_call_chain_seed(session_id, registry, CallChainSeed::default())
    }

    pub fn with_call_chain_seed(
        session_id: impl Into<String>,
        call_chain_seed: CallChainSeed,
    ) -> Self {
        tracing::debug!("Initializing minimal SkillRuntime and discovering skills...");
        let mut registry = SkillRegistry::new();
        registry.discover(Path::new("skills"));
        Self::with_registry_and_call_chain_seed(session_id, registry, call_chain_seed)
    }

    pub fn with_registry_and_call_chain_seed(
        session_id: impl Into<String>,
        registry: SkillRegistry,
        call_chain_seed: CallChainSeed,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            invocation: RwLock::new(None),
            policy: SkillToolPolicy::new(),
            registry,
            call_chain_seed,
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

    fn compatibility_notes(def: &SkillDef) -> Vec<String> {
        let mut notes = Vec::new();
        if def.meta.output_mode.is_some() {
            notes.push(
                "Legacy `output_mode` is ignored at runtime; rely on instructions and a final text response."
                    .to_string(),
            );
        }
        if def.constraints.forbid_code_write {
            notes.push(
                "Legacy `constraints.forbid_code_write` is ignored at runtime; use allowed_tools instead."
                    .to_string(),
            );
        }
        if def.constraints.required_artifact_kind.is_some() {
            notes.push(
                "Legacy `constraints.required_artifact_kind` is ignored at runtime; enforce outputs in instructions."
                    .to_string(),
            );
        }
        notes
    }

    pub async fn activate_skill(
        &self,
        def: &SkillDef,
        raw_args: Option<String>,
        json_args: Option<serde_json::Value>,
    ) -> Result<(), String> {
        if let Some(json_args) = json_args.as_ref() {
            validate_json_args(def.parameters.as_ref(), json_args)
                .map_err(|error| error.to_string())?;
        }

        let serialized_args = json_args
            .as_ref()
            .map(serde_json::Value::to_string)
            .or_else(|| raw_args.clone());
        let parent_context = self
            .call_chain_seed
            .inherited_context
            .clone()
            .unwrap_or_else(|| CallChainContext::new_root(self.session_id.clone()));
        if parent_context.contains_skill(&def.meta.name) {
            let lineage = parent_context
                .append_frame(&def.meta.name, serialized_args.as_deref())
                .lineage_names()
                .join(" -> ");
            return Err(format!(
                "Denied delegated skill run: cycle detected: {lineage}"
            ));
        }
        let call_chain_context =
            parent_context.append_frame(&def.meta.name, serialized_args.as_deref());
        if call_chain_context.current_depth() > MAX_CALL_CHAIN_DEPTH {
            return Err(format!(
                "Denied delegated skill run: max call chain depth exceeded ({})",
                MAX_CALL_CHAIN_DEPTH
            ));
        }

        let compatibility_notes = Self::compatibility_notes(def);
        for note in &compatibility_notes {
            tracing::warn!(skill = %def.meta.name, "{note}");
        }
        let allowed_tools = self.policy.canonicalize_tools(&def.meta.allowed_tools);
        if self.call_chain_seed.inherited_context.is_some()
            && allowed_tools.iter().any(|tool| tool == "ask_user_question")
        {
            return Err(format!(
                "Denied delegated skill run: skill '{}' is interactive and can only run at the top level.",
                def.meta.name
            ));
        }

        let invocation = SkillInvocation {
            skill_name: def.meta.name.clone(),
            version: def.meta.version.clone(),
            instructions: def.instructions.clone(),
            allowed_tools,
            raw_args,
            json_args,
            handoff_context: self
                .call_chain_seed
                .handoff_context
                .clone()
                .filter(|value| !value.trim().is_empty()),
            pending_interaction: None,
            state: SkillInvocationState::Running,
            call_chain_context,
            compatibility_notes,
        };

        *self.invocation.write().await = Some(invocation);
        tracing::info!("Skill '{}' activated", def.meta.name);
        Ok(())
    }

    pub async fn deactivate_skill(&self) {
        let skill_name = self
            .invocation
            .read()
            .await
            .as_ref()
            .map(|invocation| invocation.skill_name.clone());
        *self.invocation.write().await = None;
        if let Some(skill_name) = skill_name {
            tracing::info!("Skill '{}' deactivated", skill_name);
        }
    }

    pub async fn is_active(&self) -> bool {
        self.invocation.read().await.is_some()
    }

    async fn build_contract(&self) -> Option<String> {
        let invocation = self.invocation.read().await;
        let invocation = invocation.as_ref()?;

        let mut parts = Vec::new();
        parts.push(format!(
            "## Active Skill: {} v{}",
            invocation.skill_name, invocation.version
        ));
        if invocation.allowed_tools.is_empty() {
            parts.push("Allowed tools: all top-level tools".to_string());
        } else {
            parts.push(format!(
                "Allowed tools: {}",
                invocation.allowed_tools.join(", ")
            ));
        }
        parts.push(format!(
            "Interactive: {}",
            if invocation
                .allowed_tools
                .iter()
                .any(|tool| tool == "ask_user_question")
            {
                "yes"
            } else {
                "no"
            }
        ));

        if !invocation.compatibility_notes.is_empty() {
            parts.push(format!(
                "Compatibility: {}",
                invocation.compatibility_notes.join(" | ")
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
        let cmd = parts.next().unwrap_or_default();
        let skill_name = &cmd[1..];

        if skill_name.is_empty() {
            return Ok(None);
        }

        let raw_args = parts
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

        if matches!(def.meta.trigger, SkillTrigger::SuggestOnly) {
            return Err(format!(
                "Skill '{}' is suggest_only and cannot be activated manually.",
                def.meta.name
            ));
        }

        let parsed_args =
            parse_invocation_args(raw_args.as_deref()).map_err(|error| error.to_string())?;
        self.activate_skill(&def, parsed_args.raw.clone(), parsed_args.json.clone())
            .await?;

        let mut message = format!(
            "Activated skill '{}'. Follow the active skill instructions for this turn.",
            def.meta.name
        );
        if let Some(raw_args) = parsed_args.raw {
            message.push_str(&format!("\nActivation args: {}", raw_args));
        }
        if let Some(json_args) = parsed_args.json {
            message.push_str(&format!("\nActivation args (json): {}", json_args));
        }
        Ok(Some(message))
    }

    async fn consume_pending_interaction(&self) -> bool {
        let mut invocation = self.invocation.write().await;
        let Some(invocation) = invocation.as_mut() else {
            return false;
        };
        if invocation.pending_interaction.is_some() {
            invocation.pending_interaction = None;
            invocation.state = SkillInvocationState::Running;
            tracing::info!(skill = %invocation.skill_name, "Skill resumed after user reply");
            true
        } else {
            false
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
        if self.consume_pending_interaction().await {
            return ExtensionDecision::Continue;
        }

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

        let invocation = self.invocation.read().await;
        if let Some(invocation) = invocation.as_ref() {
            draft.skill_state_summary = Some(invocation.state_summary());

            let mut blocks = vec![Self::truncate_for_prompt(&invocation.instructions, 4_000)];
            if let Some(args_section) = format_prompt_argument_sections(
                invocation.raw_args.as_deref(),
                invocation.json_args.as_ref(),
            ) {
                blocks.push(args_section);
            }
            if let Some(handoff_context) = invocation
                .handoff_context
                .as_ref()
                .filter(|value| !value.trim().is_empty())
            {
                blocks.push(format!(
                    "## Skill Handoff Context\n{}",
                    Self::truncate_for_prompt(handoff_context, 2_000)
                ));
            }
            draft.skill_instructions = Some(blocks.join("\n\n"));
        }

        draft
    }

    async fn before_tool_resolution(&self, tools: Vec<Arc<dyn Tool>>) -> Vec<Arc<dyn Tool>> {
        let invocation = self.invocation.read().await;
        let Some(invocation) = invocation.as_ref() else {
            return tools;
        };

        if invocation.allowed_tools.is_empty() {
            return tools;
        }

        self.policy
            .filter_tools_by_allowed_names(tools, &invocation.allowed_tools)
    }

    async fn enrich_tool_context(
        &self,
        mut ctx: crate::tools::ToolContext,
    ) -> crate::tools::ToolContext {
        if let Some(invocation) = self.invocation.read().await.as_ref() {
            ctx.active_skill_name = Some(invocation.skill_name.clone());
            ctx.call_chain_context = Some(invocation.call_chain_context.clone());
        } else if let Some(inherited_context) = self.call_chain_seed.inherited_context.as_ref() {
            ctx.call_chain_context = Some(inherited_context.clone());
        }

        if let Some(inherited_steps) = self.call_chain_seed.inherited_budget.remaining_steps {
            ctx.call_chain_budget.remaining_steps = Some(
                ctx.call_chain_budget
                    .remaining_steps
                    .unwrap_or(inherited_steps)
                    .min(inherited_steps)
                    .max(1),
            );
        }
        if let Some(inherited_timeout_sec) =
            self.call_chain_seed.inherited_budget.remaining_timeout_sec
        {
            ctx.call_chain_budget.remaining_timeout_sec = Some(
                ctx.call_chain_budget
                    .remaining_timeout_sec
                    .unwrap_or(inherited_timeout_sec)
                    .min(inherited_timeout_sec)
                    .max(1),
            );
        }
        ctx
    }

    async fn after_tool_result(&self, result: &ToolExecutionEnvelope) {
        let Some(request) = result.effects.await_user.as_ref() else {
            return;
        };

        let mut invocation = self.invocation.write().await;
        let Some(invocation) = invocation.as_mut() else {
            return;
        };

        invocation.pending_interaction = Some(PendingInteraction {
            context_key: request.context_key.clone(),
            question: request.question.clone(),
            options: request.options.clone(),
            recommendation: request.recommendation.clone(),
            asked_at: chrono::Utc::now().to_rfc3339(),
        });
        invocation.state = SkillInvocationState::WaitingUser;
    }

    async fn before_finish(&self) -> FinishDecision {
        FinishDecision::Allow
    }

    async fn on_finish_committed(&self, _summary: &str) {
        self.deactivate_skill().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call_chain::{CallChainBudget, CallChainSeed};
    use crate::core::extensions::{ExecutionExtension, PromptDraft};
    use crate::skills::definition::{ArtifactKind, OutputMode, SkillConstraints, SkillMeta};
    use std::sync::atomic::Ordering;

    fn make_test_skill(allowed_tools: &[&str]) -> SkillDef {
        SkillDef {
            meta: SkillMeta {
                name: "test_skill".to_string(),
                version: "1.0".to_string(),
                description: "Test skill".to_string(),
                trigger: SkillTrigger::ManualOnly,
                allowed_tools: allowed_tools
                    .iter()
                    .map(|tool| (*tool).to_string())
                    .collect(),
                output_mode: None,
            },
            instructions: "Do the thing.".to_string(),
            parameters: None,
            constraints: SkillConstraints::default(),
        }
    }

    #[tokio::test]
    async fn test_activate_deactivate_lifecycle() {
        let rt = SkillRuntime::new();
        assert!(!rt.is_active().await);

        let skill = make_test_skill(&["read_file"]);
        rt.activate_skill(&skill, None, None).await.unwrap();
        assert!(rt.is_active().await);

        rt.deactivate_skill().await;
        assert!(!rt.is_active().await);
    }

    #[tokio::test]
    async fn test_activate_with_raw_and_json_args_injects_prompt_sections() {
        let rt = SkillRuntime::new();
        let mut skill = make_test_skill(&["read_file"]);
        skill.parameters = Some(serde_json::json!({
            "path": { "type": "string", "required": true }
        }));
        rt.activate_skill(
            &skill,
            Some("src/lib.rs".to_string()),
            Some(serde_json::json!({ "path": "src/lib.rs" })),
        )
        .await
        .unwrap();

        let draft = rt.before_prompt_build(PromptDraft::default()).await;
        let summary = draft.skill_state_summary.unwrap();
        let instructions = draft.skill_instructions.unwrap();
        assert!(summary.contains("Activation args (raw): src/lib.rs"));
        assert!(summary.contains("\"path\":\"src/lib.rs\""));
        assert!(instructions.contains("## Skill Arguments (JSON)"));
        assert!(instructions.contains("## Skill Arguments (Raw)"));
    }

    #[tokio::test]
    async fn test_activate_skill_rejects_invalid_json_args() {
        let rt = SkillRuntime::new();
        let mut skill = make_test_skill(&["read_file"]);
        skill.parameters = Some(serde_json::json!({
            "path": { "type": "string", "required": true }
        }));

        let error = rt
            .activate_skill(&skill, None, Some(serde_json::json!({ "path": 42 })))
            .await
            .unwrap_err();
        assert!(error.contains("parameter 'path' must be of type string"));
    }

    #[tokio::test]
    async fn test_prompt_build_injects_contract_and_compatibility_notes() {
        let rt = SkillRuntime::new();
        let mut skill = make_test_skill(&["read_file"]);
        skill.meta.output_mode = Some(OutputMode::ReviewOnly);
        skill.constraints = SkillConstraints {
            forbid_code_write: true,
            required_artifact_kind: Some(ArtifactKind::ReviewReport),
        };
        rt.activate_skill(&skill, None, None).await.unwrap();

        let draft = rt.before_prompt_build(PromptDraft::default()).await;
        let contract = draft.skill_contract.unwrap();
        assert!(contract.contains("Allowed tools: read_file"));
        assert!(contract.contains("Compatibility:"));
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
        let mut skill = make_test_skill(&["read_file"]);
        skill.instructions = "你".repeat(4_001);
        rt.activate_skill(&skill, None, None).await.unwrap();

        let draft = rt.before_prompt_build(PromptDraft::default()).await;
        let instructions = draft.skill_instructions.unwrap();
        assert!(instructions.ends_with("...[truncated]"));
        assert!(instructions.starts_with('你'));
    }

    #[tokio::test]
    async fn test_allowed_tools_filtering_only_keeps_whitelist_and_runtime_tools() {
        let rt = SkillRuntime::new();
        let skill = make_test_skill(&["read_file", "ask_user_question"]);
        rt.activate_skill(&skill, None, None).await.unwrap();

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
            Arc::new(MockTool("ask_user_question".to_string())),
            Arc::new(MockTool("task_plan".to_string())),
        ];

        let filtered = rt.before_tool_resolution(tools).await;
        let names: Vec<String> = filtered.iter().map(|tool| tool.name()).collect();
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"ask_user_question".to_string()));
        assert!(names.contains(&"task_plan".to_string()));
        assert!(!names.contains(&"write_file".to_string()));
    }

    #[tokio::test]
    async fn test_resume_with_pending_interaction() {
        let rt = SkillRuntime::new();
        let skill = make_test_skill(&["read_file", "ask_user_question"]);
        rt.activate_skill(&skill, None, None).await.unwrap();

        {
            let mut invocation = rt.invocation.write().await;
            let invocation = invocation.as_mut().unwrap();
            invocation.pending_interaction = Some(PendingInteraction {
                context_key: "project_name".to_string(),
                question: "What is the project name?".to_string(),
                options: vec![],
                recommendation: None,
                asked_at: "now".to_string(),
            });
            invocation.state = SkillInvocationState::WaitingUser;
        }

        assert!(matches!(
            rt.before_turn_start("MyProject").await,
            ExtensionDecision::Continue
        ));

        let invocation = rt.invocation.read().await;
        let invocation = invocation.as_ref().unwrap();
        assert_eq!(invocation.state, SkillInvocationState::Running);
        assert!(invocation.pending_interaction.is_none());
    }

    #[tokio::test]
    async fn test_enrich_tool_context_inherits_seeded_context_and_budget() {
        let inherited_context = CallChainContext::new_root("root").append_frame("planner", None);
        inherited_context.total_calls.store(2, Ordering::SeqCst);
        let rt = SkillRuntime::with_registry_and_call_chain_seed(
            "seeded-session",
            SkillRegistry::new(),
            CallChainSeed {
                inherited_context: Some(inherited_context.clone()),
                inherited_budget: CallChainBudget {
                    remaining_steps: Some(3),
                    remaining_timeout_sec: Some(7),
                },
                handoff_context: Some("Inspect parser flow.".to_string()),
            },
        );

        let mut ctx = crate::tools::ToolContext::new("seeded-session", "cli");
        ctx.call_chain_budget = CallChainBudget {
            remaining_steps: Some(9),
            remaining_timeout_sec: Some(15),
        };

        let enriched = rt.enrich_tool_context(ctx).await;
        let propagated = enriched
            .call_chain_context
            .expect("inherited call context should be present");

        assert!(enriched.active_skill_name.is_none());
        assert_eq!(propagated.lineage_names(), vec!["planner".to_string()]);
        assert_eq!(propagated.root_session_id, "root");
        assert_eq!(enriched.call_chain_budget.remaining_steps, Some(3));
        assert_eq!(enriched.call_chain_budget.remaining_timeout_sec, Some(7));

        propagated.total_calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(inherited_context.total_calls_used(), 3);
    }

    #[tokio::test]
    async fn test_before_finish_always_allows() {
        let rt = SkillRuntime::new();
        let skill = make_test_skill(&["read_file"]);
        rt.activate_skill(&skill, None, None).await.unwrap();
        assert!(matches!(rt.before_finish().await, FinishDecision::Allow));
    }

    #[tokio::test]
    async fn test_before_turn_start_activates_skill_from_command_with_json_args() {
        let mut registry = SkillRegistry::new();
        let mut skill = make_test_skill(&["read_file"]);
        skill.parameters = Some(serde_json::json!({
            "path": { "type": "string", "required": true }
        }));
        registry.insert(skill);
        let rt = SkillRuntime::with_registry(registry);

        let decision = rt
            .before_turn_start(r#"/test_skill {"path":"src/lib.rs"}"#)
            .await;
        match decision {
            ExtensionDecision::Intercept { prompt_overlay } => {
                let overlay = prompt_overlay.expect("expected overlay");
                assert!(overlay.contains("Activated skill 'test_skill'"));
                assert!(overlay.contains("Activation args (json)"));
                assert!(rt.is_active().await);
            }
            other => panic!("expected intercept, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_before_turn_start_rejects_invalid_json_args() {
        let mut registry = SkillRegistry::new();
        let mut skill = make_test_skill(&["read_file"]);
        skill.parameters = Some(serde_json::json!({
            "path": { "type": "string", "required": true }
        }));
        registry.insert(skill);
        let rt = SkillRuntime::with_registry(registry);

        let decision = rt.before_turn_start(r#"/test_skill {"path":42}"#).await;
        assert!(matches!(decision, ExtensionDecision::Halt { .. }));
    }

    #[tokio::test]
    async fn test_delegated_skill_rejects_interactive_allowed_tools() {
        let mut registry = SkillRegistry::new();
        let mut skill = make_test_skill(&["ask_user_question"]);
        skill.meta.name = "interactive".to_string();
        registry.insert(skill);
        let rt = SkillRuntime::with_registry_and_call_chain_seed(
            "sub-session",
            registry,
            CallChainSeed {
                inherited_context: Some(CallChainContext::new_root("parent")),
                inherited_budget: CallChainBudget::default(),
                handoff_context: None,
            },
        );

        let decision = rt.before_turn_start("/interactive").await;
        match decision {
            ExtensionDecision::Halt { message } => {
                assert!(message.contains("interactive"));
                assert!(message.contains("top level"));
            }
            other => panic!("expected halt, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_before_turn_start_rejects_manual_activation_of_suggest_only_skill() {
        let mut registry = SkillRegistry::new();
        let mut skill = make_test_skill(&["read_file"]);
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
        let skill = make_test_skill(&["read_file", "ask_user_question"]);
        rt.activate_skill(&skill, None, None).await.unwrap();

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

        let invocation = rt.invocation.read().await;
        let invocation = invocation.as_ref().unwrap();
        assert_eq!(invocation.state, SkillInvocationState::WaitingUser);
        assert_eq!(
            invocation
                .pending_interaction
                .as_ref()
                .map(|pending| pending.context_key.as_str()),
            Some("goal")
        );
    }

    #[tokio::test]
    async fn test_on_finish_committed_deactivates_skill() {
        let rt = SkillRuntime::new();
        let skill = make_test_skill(&["read_file"]);
        rt.activate_skill(&skill, None, None).await.unwrap();
        rt.on_finish_committed("done").await;
        assert!(!rt.is_active().await);
    }
}
