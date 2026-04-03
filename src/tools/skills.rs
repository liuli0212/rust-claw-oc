use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::Value;
use tracing::Instrument;

use super::protocol::{clean_schema, StructuredToolOutput, Tool, ToolContext, ToolError};
use crate::event_log::{AgentEvent, EventLog};
use crate::skills::call_tree::{
    SkillBudget, SkillCallContext, SkillSessionSeed, MAX_DELEGATION_CALLS_PER_ROOT_REQUEST,
};
use crate::skills::policy::SkillToolPolicy;
use crate::skills::registry::SkillRegistry;

pub struct CallSkillTool {
    llm: Arc<dyn crate::llm_client::LlmClient>,
    base_tools: Vec<Arc<dyn Tool>>,
    registry: SkillRegistry,
    policy: SkillToolPolicy,
}

const MAX_SKILL_CALL_DEPTH: usize = 3;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CallSkillArgs {
    /// The name of the skill to call (e.g., "summarize_info").
    pub skill_name: String,
    /// Arguments to pass to the skill.
    pub args: Option<String>,
    /// A summary of the current context to provide to the sub-agent.
    pub input_summary: String,
    /// Optional list of tools to allow for the sub-agent.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Optional timeout in seconds.
    pub timeout_sec: Option<u64>,
    /// Optional maximum steps for the sub-agent.
    pub max_steps: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillCallFailureKind {
    MissingTools,
    BudgetExceeded,
    Timeout,
    CycleDetected,
    DepthExceeded,
    PolicyDenied,
    ChildExecutionFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillCallFailure {
    pub kind: SkillCallFailureKind,
    pub message: String,
    pub retryable: bool,
    pub llm_action_hint: Option<String>,
    pub details: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillCallResult {
    pub ok: bool,
    pub skill_name: String,
    pub summary: String,
    pub findings: Vec<String>,
    pub artifacts: Vec<String>,
    pub lineage: Vec<String>,
    pub effective_tools: Vec<String>,
    pub effective_max_steps: usize,
    pub effective_timeout_sec: u64,
    pub sub_session_id: Option<String>,
    pub transcript_path: Option<String>,
    pub event_log_path: Option<String>,
    pub failure: Option<SkillCallFailure>,
}

impl CallSkillTool {
    pub fn new(llm: Arc<dyn crate::llm_client::LlmClient>, base_tools: Vec<Arc<dyn Tool>>) -> Self {
        let mut registry = SkillRegistry::new();
        registry.discover(Path::new("skills"));
        Self {
            llm,
            base_tools,
            registry,
            policy: SkillToolPolicy::new(),
        }
    }

    async fn append_event(
        &self,
        tool_ctx: Option<&ToolContext>,
        session_id: &str,
        event_type: &str,
        payload: serde_json::Value,
    ) {
        let _ = EventLog::new(session_id)
            .append(AgentEvent::new(
                event_type,
                session_id.to_string(),
                None,
                None,
                payload.clone(),
            ))
            .await;
        if let Some(tool_ctx) = tool_ctx {
            if let Some(trace) = &tool_ctx.trace {
                let status = if event_type.contains("denied") {
                    crate::trace::TraceStatus::Skipped
                } else if event_type.contains("finished") {
                    crate::trace::TraceStatus::Ok
                } else {
                    crate::trace::TraceStatus::Running
                };
                crate::trace::shared_bus().record_event(
                    &crate::trace::TraceContext {
                        trace_id: trace.trace_id.clone(),
                        run_id: trace.run_id.clone(),
                        session_id: tool_ctx.session_id.clone(),
                        root_session_id: trace.root_session_id.clone(),
                        task_id: trace.task_id.clone(),
                        turn_id: trace.turn_id.clone(),
                        iteration: trace.iteration,
                        parent_span_id: trace.parent_span_id.clone(),
                    },
                    crate::trace::TraceActor::Skill,
                    event_type,
                    status,
                    Some(event_type.to_string()),
                    payload,
                );
            }
        }
    }

    fn canonicalize_tools(&self, tools: &[String]) -> Vec<String> {
        let mut canonical = Vec::new();
        for tool in tools {
            let mapped = self.policy.canonical_name(tool);
            if !canonical.contains(&mapped) {
                canonical.push(mapped);
            }
        }
        canonical
    }

    fn failure(
        &self,
        kind: SkillCallFailureKind,
        message: impl Into<String>,
        retryable: bool,
        llm_action_hint: Option<&str>,
        details: serde_json::Value,
    ) -> SkillCallFailure {
        SkillCallFailure {
            kind,
            message: message.into(),
            retryable,
            llm_action_hint: llm_action_hint.map(str::to_string),
            details,
        }
    }
}

pub struct SkillFailureContext<'a> {
    pub tool_ctx: &'a ToolContext,
    pub skill_name: &'a str,
    pub lineage: Vec<String>,
    pub effective_tools: Vec<String>,
    pub effective_max_steps: usize,
    pub effective_timeout_sec: u64,
    pub failure: SkillCallFailure,
    pub event_type: &'a str,
}

impl CallSkillTool {
    async fn failure_output(&self, f_ctx: SkillFailureContext<'_>) -> Result<String, ToolError> {
        let payload = json!({
            "skill_name": f_ctx.skill_name,
            "lineage": f_ctx.lineage,
            "failure": &f_ctx.failure,
            "effective_tools": f_ctx.effective_tools,
            "effective_max_steps": f_ctx.effective_max_steps,
            "effective_timeout_sec": f_ctx.effective_timeout_sec,
        });
        self.append_event(
            Some(f_ctx.tool_ctx),
            &f_ctx.tool_ctx.session_id,
            f_ctx.event_type,
            payload,
        )
        .await;

        let summary = f_ctx.failure.message.clone();
        StructuredToolOutput::new(
            "call_skill",
            false,
            serde_json::to_string_pretty(&SkillCallResult {
                ok: false,
                skill_name: f_ctx.skill_name.to_string(),
                summary,
                findings: Vec::new(),
                artifacts: Vec::new(),
                lineage: f_ctx.lineage,
                effective_tools: f_ctx.effective_tools,
                effective_max_steps: f_ctx.effective_max_steps,
                effective_timeout_sec: f_ctx.effective_timeout_sec,
                sub_session_id: None,
                transcript_path: None,
                event_log_path: None,
                failure: Some(f_ctx.failure),
            })
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?,
            None,
            None,
            false,
        )
        .to_json_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm_client::{LlmClient, LlmError, StreamEvent};
    use crate::skills::definition::{SkillConstraints, SkillDef, SkillMeta, SkillTrigger};
    use crate::tools::protocol::ToolExecutionEnvelope;
    use tokio::sync::mpsc;

    struct DummyLlm;

    #[async_trait]
    impl LlmClient for DummyLlm {
        fn model_name(&self) -> &str {
            "dummy"
        }

        fn provider_name(&self) -> &str {
            "dummy"
        }

        async fn stream(
            &self,
            _: Vec<crate::context::Message>,
            _: Option<crate::context::Message>,
            _: Vec<Arc<dyn Tool>>,
        ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        }
    }

    struct FinishImmediatelyLlm;

    #[async_trait]
    impl LlmClient for FinishImmediatelyLlm {
        fn model_name(&self) -> &str {
            "finish-immediately"
        }

        fn provider_name(&self) -> &str {
            "dummy"
        }

        async fn stream(
            &self,
            _: Vec<crate::context::Message>,
            _: Option<crate::context::Message>,
            _: Vec<Arc<dyn Tool>>,
        ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
            let (tx, rx) = mpsc::channel(4);
            let _ = tx.try_send(StreamEvent::ToolCall(
                crate::context::FunctionCall {
                    name: "finish_task".to_string(),
                    args: json!({ "summary": "done" }),
                    id: Some("tc_finish".to_string()),
                },
                None,
            ));
            let _ = tx.try_send(StreamEvent::Done);
            Ok(rx)
        }
    }

    struct MockTool(&'static str);

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> String {
            self.0.to_string()
        }

        fn description(&self) -> String {
            String::new()
        }

        fn parameters_schema(&self) -> Value {
            json!({})
        }

        async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
            Ok(String::new())
        }
    }

    fn make_skill(name: &str, allowed_tools: &[&str]) -> SkillDef {
        SkillDef {
            meta: SkillMeta {
                name: name.to_string(),
                version: "1.0".to_string(),
                description: "test skill".to_string(),
                trigger: SkillTrigger::ManualOnly,
                allowed_tools: allowed_tools
                    .iter()
                    .map(|tool| (*tool).to_string())
                    .collect(),
                output_mode: None,
            },
            instructions: "do the thing".to_string(),
            parameters: None,
            constraints: SkillConstraints {
                ..SkillConstraints::default()
            },
        }
    }

    fn make_tool_with_base_tools(
        registry: SkillRegistry,
        llm: Arc<dyn LlmClient>,
        base_tools: Vec<Arc<dyn Tool>>,
    ) -> CallSkillTool {
        CallSkillTool {
            llm,
            base_tools,
            registry,
            policy: SkillToolPolicy::new(),
        }
    }

    fn make_tool_with_llm(registry: SkillRegistry, llm: Arc<dyn LlmClient>) -> CallSkillTool {
        make_tool_with_base_tools(
            registry,
            llm,
            vec![
                Arc::new(MockTool("read_file")),
                Arc::new(MockTool("call_skill")),
            ],
        )
    }

    fn make_tool(registry: SkillRegistry) -> CallSkillTool {
        make_tool_with_llm(registry, Arc::new(DummyLlm))
    }

    fn make_ctx(
        active_skill: Option<&str>,
        lineage: &[&str],
        visible_tools: &[&str],
        remaining_steps: usize,
        remaining_timeout_sec: u64,
    ) -> ToolContext {
        let mut ctx = ToolContext::new("parent", "cli");
        ctx.active_skill_name = active_skill.map(str::to_string);
        ctx.visible_tools = visible_tools
            .iter()
            .map(|tool| (*tool).to_string())
            .collect();
        if !lineage.is_empty() {
            let mut call_context = SkillCallContext::new_root("root");
            for skill in lineage {
                call_context = call_context.append_frame(skill, None);
            }
            ctx.skill_call_context = Some(call_context);
        }
        ctx.skill_budget = SkillBudget {
            remaining_steps: Some(remaining_steps),
            remaining_timeout_sec: Some(remaining_timeout_sec),
        };
        ctx
    }

    fn parse_result(output: &str) -> SkillCallResult {
        let envelope: ToolExecutionEnvelope = serde_json::from_str(output).unwrap();
        serde_json::from_str(&envelope.result.output).unwrap()
    }

    #[tokio::test]
    async fn test_call_skill_allows_non_skill_parent_sessions() {
        let mut registry = SkillRegistry::new();
        registry.insert(make_skill("child_skill", &[]));
        let tool = make_tool_with_llm(registry, Arc::new(FinishImmediatelyLlm));
        let ctx = make_ctx(None, &[], &["call_skill"], 8, 20);

        let output = tool
            .execute(
                serde_json::json!({
                    "skill_name": "child_skill",
                    "input_summary": "summary"
                }),
                &ctx,
            )
            .await
            .unwrap();

        let payload = parse_result(&output);
        assert!(payload.ok);
        assert_eq!(payload.skill_name, "child_skill");
        assert_eq!(payload.lineage, vec!["child_skill"]);
    }

    #[tokio::test]
    async fn test_call_skill_denies_cycle_with_full_lineage() {
        let tool = make_tool(SkillRegistry::new());
        let ctx = make_ctx(Some("alpha"), &["alpha", "beta"], &["call_skill"], 8, 20);

        let output = tool
            .execute(
                serde_json::json!({
                    "skill_name": "alpha",
                    "input_summary": "summary"
                }),
                &ctx,
            )
            .await
            .unwrap();

        let payload = parse_result(&output);
        assert!(!payload.ok);
        assert_eq!(payload.lineage, vec!["alpha", "beta", "alpha"]);
        let failure = payload.failure.unwrap();
        assert!(matches!(failure.kind, SkillCallFailureKind::CycleDetected));
        assert!(failure.message.contains("alpha -> beta -> alpha"));
    }

    #[tokio::test]
    async fn test_call_skill_fails_fast_when_child_requires_missing_tools() {
        let mut registry = SkillRegistry::new();
        registry.insert(make_skill("child_skill", &["read_file", "write_file"]));
        let tool = make_tool(registry);
        let ctx = make_ctx(
            Some("planner"),
            &["planner"],
            &["read_file", "call_skill"],
            12,
            20,
        );

        let output = tool
            .execute(
                serde_json::json!({
                    "skill_name": "child_skill",
                    "input_summary": "summary",
                    "max_steps": 10,
                    "timeout_sec": 120
                }),
                &ctx,
            )
            .await
            .unwrap();

        let payload = parse_result(&output);
        assert!(!payload.ok);
        assert_eq!(payload.effective_max_steps, 6);
        assert_eq!(payload.effective_timeout_sec, 20);
        let failure = payload.failure.unwrap();
        assert!(matches!(failure.kind, SkillCallFailureKind::MissingTools));
        assert!(failure.message.contains("write_file"));
    }

    #[tokio::test]
    async fn test_call_skill_denies_when_depth_limit_is_exceeded() {
        let tool = make_tool(SkillRegistry::new());
        let ctx = make_ctx(
            Some("gamma"),
            &["alpha", "beta", "gamma"],
            &["call_skill"],
            12,
            20,
        );

        let output = tool
            .execute(
                serde_json::json!({
                    "skill_name": "delta",
                    "input_summary": "summary"
                }),
                &ctx,
            )
            .await
            .unwrap();

        let payload = parse_result(&output);
        assert!(!payload.ok);
        assert_eq!(payload.lineage, vec!["alpha", "beta", "gamma", "delta"]);
        let failure = payload.failure.unwrap();
        assert!(matches!(failure.kind, SkillCallFailureKind::DepthExceeded));
        assert!(failure.message.contains("max nested skill depth exceeded"));
    }

    #[tokio::test]
    async fn test_call_skill_denies_when_shared_total_call_budget_is_exhausted() {
        let tool = make_tool(SkillRegistry::new());
        let ctx = make_ctx(Some("planner"), &["planner"], &["call_skill"], 12, 20);
        ctx.skill_call_context
            .as_ref()
            .unwrap()
            .total_skill_calls
            .store(MAX_DELEGATION_CALLS_PER_ROOT_REQUEST, Ordering::SeqCst);

        let output = tool
            .execute(
                serde_json::json!({
                    "skill_name": "child_skill",
                    "input_summary": "summary"
                }),
                &ctx,
            )
            .await
            .unwrap();

        let payload = parse_result(&output);
        assert!(!payload.ok);
        let failure = payload.failure.unwrap();
        assert!(matches!(failure.kind, SkillCallFailureKind::BudgetExceeded));
        assert!(failure
            .message
            .contains("max total nested skill calls exceeded"));
    }

    #[tokio::test]
    async fn test_call_skill_allows_child_that_requests_subagent_when_available() {
        let mut registry = SkillRegistry::new();
        registry.insert(make_skill("child_skill", &["spawn_subagent"]));
        let tool = make_tool_with_base_tools(
            registry,
            Arc::new(FinishImmediatelyLlm),
            vec![
                Arc::new(MockTool("read_file")),
                Arc::new(MockTool("call_skill")),
                Arc::new(MockTool("subagent")),
            ],
        );
        let ctx = make_ctx(
            Some("planner"),
            &["planner"],
            &["subagent", "call_skill"],
            12,
            20,
        );

        let output = tool
            .execute(
                serde_json::json!({
                    "skill_name": "child_skill",
                    "input_summary": "summary"
                }),
                &ctx,
            )
            .await
            .unwrap();

        let payload = parse_result(&output);
        assert!(payload.ok);
        assert!(payload.effective_tools.contains(&"subagent".to_string()));
    }

    #[tokio::test]
    async fn test_call_skill_denies_child_that_requests_unavailable_subagent_tool() {
        let mut registry = SkillRegistry::new();
        registry.insert(make_skill("child_skill", &["spawn_subagent"]));
        let tool = make_tool(registry);
        let ctx = make_ctx(
            Some("planner"),
            &["planner"],
            &["subagent", "call_skill"],
            12,
            20,
        );

        let output = tool
            .execute(
                serde_json::json!({
                    "skill_name": "child_skill",
                    "input_summary": "summary"
                }),
                &ctx,
            )
            .await
            .unwrap();

        let payload = parse_result(&output);
        assert!(!payload.ok);
        let failure = payload.failure.unwrap();
        assert!(matches!(failure.kind, SkillCallFailureKind::MissingTools));
        assert!(failure.message.contains("subagent"));
    }
}

#[async_trait]
impl Tool for CallSkillTool {
    fn name(&self) -> String {
        "call_skill".to_string()
    }

    fn description(&self) -> String {
        "Call another skill by dispatching a sub-agent to execute it. \
         This is the preferred way for one skill to delegate work to another."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        clean_schema(serde_json::to_value(schema_for!(CallSkillArgs)).unwrap())
    }

    fn has_side_effects(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let parsed: CallSkillArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        if parsed.skill_name.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "skill_name must not be empty".to_string(),
            ));
        }

        let target_skill = parsed.skill_name.clone();
        let input_summary = parsed.input_summary.clone();
        let requested_args = parsed.args.clone();
        let caller_label = ctx
            .active_skill_name
            .clone()
            .unwrap_or_else(|| ctx.session_id.clone());

        let parent_context = ctx
            .skill_call_context
            .clone()
            .unwrap_or_else(|| SkillCallContext::new_root(ctx.session_id.clone()));
        let requested_timeout_sec = parsed.timeout_sec.unwrap_or(120).max(1);
        let parent_remaining_steps = ctx.skill_budget.remaining_steps.unwrap_or(1).max(1);
        let requested_steps = parsed.max_steps.unwrap_or(parent_remaining_steps).max(1);
        let effective_max_steps = requested_steps.min((parent_remaining_steps / 2).max(1));
        let parent_remaining_timeout_sec = ctx
            .skill_budget
            .remaining_timeout_sec
            .unwrap_or(requested_timeout_sec)
            .max(1);
        let effective_timeout_sec = requested_timeout_sec
            .min(parent_remaining_timeout_sec)
            .max(1);
        let child_context_preview =
            parent_context.append_frame(&target_skill, requested_args.as_deref());
        let child_lineage = child_context_preview.lineage_names();

        self.append_event(
            Some(ctx),
            &ctx.session_id,
            "skill_call_requested",
            json!({
                "caller": &caller_label,
                "target_skill": &target_skill,
                "lineage": &child_lineage,
                "requested_max_steps": requested_steps,
                "requested_timeout_sec": requested_timeout_sec,
                "parent_remaining_steps": parent_remaining_steps,
                "parent_remaining_timeout_sec": parent_remaining_timeout_sec,
            }),
        )
        .await;

        if parent_context.contains_skill(&target_skill) {
            let failure = self.failure(
                SkillCallFailureKind::CycleDetected,
                format!(
                    "Denied skill call: cycle detected: {}",
                    child_lineage.join(" -> ")
                ),
                false,
                Some("Do not retry the same child skill. Choose a different skill or summarize the constraint."),
                json!({
                    "target_skill": &target_skill,
                    "lineage": &child_lineage,
                }),
            );
            return self
                .failure_output(SkillFailureContext {
                    tool_ctx: ctx,
                    skill_name: &target_skill,
                    lineage: child_lineage,
                    effective_tools: Vec::new(),
                    effective_max_steps,
                    effective_timeout_sec,
                    failure,
                    event_type: "skill_call_denied_cycle",
                })
                .await;
        }

        if child_context_preview.current_depth() > MAX_SKILL_CALL_DEPTH {
            let failure = self.failure(
                SkillCallFailureKind::DepthExceeded,
                format!(
                    "Denied skill call: max nested skill depth exceeded ({})",
                    MAX_SKILL_CALL_DEPTH
                ),
                false,
                Some("Flatten the plan or delegate to a sibling skill instead of nesting deeper."),
                json!({
                    "target_skill": &target_skill,
                    "lineage": &child_lineage,
                    "max_depth": MAX_SKILL_CALL_DEPTH,
                }),
            );
            return self
                .failure_output(SkillFailureContext {
                    tool_ctx: ctx,
                    skill_name: &target_skill,
                    lineage: child_lineage,
                    effective_tools: Vec::new(),
                    effective_max_steps,
                    effective_timeout_sec,
                    failure,
                    event_type: "skill_call_denied_depth",
                })
                .await;
        }

        if parent_context.total_skill_calls_used() >= MAX_DELEGATION_CALLS_PER_ROOT_REQUEST {
            let failure = self.failure(
                SkillCallFailureKind::BudgetExceeded,
                format!(
                    "Denied skill call: max total nested skill calls exceeded ({})",
                    MAX_DELEGATION_CALLS_PER_ROOT_REQUEST
                ),
                false,
                Some("Do not spawn more child skills. Summarize findings or finish with the work already completed."),
                json!({
                    "target_skill": &target_skill,
                    "lineage": &child_lineage,
                    "total_skill_calls_used": parent_context.total_skill_calls_used(),
                    "max_total_skill_calls": MAX_DELEGATION_CALLS_PER_ROOT_REQUEST,
                }),
            );
            return self
                .failure_output(SkillFailureContext {
                    tool_ctx: ctx,
                    skill_name: &target_skill,
                    lineage: child_lineage,
                    effective_tools: Vec::new(),
                    effective_max_steps,
                    effective_timeout_sec,
                    failure,
                    event_type: "skill_call_denied_budget",
                })
                .await;
        }

        let Some(def) = self.registry.clone_skill(&target_skill) else {
            let failure = self.failure(
                SkillCallFailureKind::PolicyDenied,
                format!("Denied skill call: unknown skill '{}'", target_skill),
                false,
                Some("Choose one of the available skills or continue without delegating."),
                json!({
                    "target_skill": &target_skill,
                    "available_skills": self.registry.names(),
                }),
            );
            return self
                .failure_output(SkillFailureContext {
                    tool_ctx: ctx,
                    skill_name: &target_skill,
                    lineage: child_lineage,
                    effective_tools: Vec::new(),
                    effective_max_steps,
                    effective_timeout_sec,
                    failure,
                    event_type: "skill_call_denied_policy",
                })
                .await;
        };

        let callee_declared_tools = self.canonicalize_tools(&def.meta.allowed_tools);
        let runtime_available_tools: Vec<String> =
            self.base_tools.iter().map(|tool| tool.name()).collect();
        let runtime_available_tools = self.canonicalize_tools(&runtime_available_tools);

        let effective_tools = if callee_declared_tools.is_empty() {
            runtime_available_tools.clone()
        } else {
            callee_declared_tools
                .iter()
                .filter(|name| runtime_available_tools.contains(name))
                .cloned()
                .collect::<Vec<_>>()
        };

        let mut effective_tools = effective_tools;
        if effective_tools.is_empty() {
            effective_tools.push("__skill_no_tools__".to_string());
        }
        let missing_tools: Vec<String> = callee_declared_tools
            .iter()
            .filter(|name| !runtime_available_tools.contains(name))
            .cloned()
            .collect();

        if !missing_tools.is_empty() {
            let failure = self.failure(
                SkillCallFailureKind::MissingTools,
                format!(
                    "Denied skill call: child requires tools unavailable in the current runtime: [{}]",
                    missing_tools.join(", ")
                ),
                false,
                Some("Do not retry this child skill until those tools are available, or choose a different skill."),
                json!({
                    "target_skill": &target_skill,
                    "lineage": &child_lineage,
                    "missing_tools": &missing_tools,
                    "runtime_available_tools": &runtime_available_tools,
                }),
            );
            return self
                .failure_output(SkillFailureContext {
                    tool_ctx: ctx,
                    skill_name: &target_skill,
                    lineage: child_lineage,
                    effective_tools: effective_tools.clone(),
                    effective_max_steps,
                    effective_timeout_sec,
                    failure,
                    event_type: "skill_call_denied_policy",
                })
                .await;
        }

        let goal = if let Some(skill_args) = requested_args.as_ref() {
            format!("/{} {}", target_skill, skill_args)
        } else {
            format!("/{}", target_skill)
        };

        tracing::info!(
            root_session_id = %parent_context.root_session_id,
            parent_session_id = %ctx.session_id,
            target_skill = %target_skill,
            depth = child_context_preview.current_depth(),
            lineage = %child_lineage.join(" -> "),
            remaining_steps = effective_max_steps,
            remaining_timeout_sec = effective_timeout_sec,
            "Calling skill via subagent"
        );

        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_notify = Arc::new(tokio::sync::Notify::new());
        let session_allowed_tools = effective_tools.clone();
        let allow_subagent_tool = session_allowed_tools
            .iter()
            .any(|tool_name| tool_name == "subagent");

        let built = crate::session::factory::build_subagent_session(
            ctx,
            self.llm.clone(),
            &self.base_tools,
            crate::session::factory::SubagentSessionConfig {
                sub_session_id: None,
                allowed_tools: session_allowed_tools,
                energy_budget: effective_max_steps,
                timeout_sec: effective_timeout_sec,
                parent_context_text: input_summary.clone(),
                skill_session_seed: SkillSessionSeed {
                    inherited_call_context: Some(parent_context.clone()),
                    inherited_budget: SkillBudget {
                        remaining_steps: Some(effective_max_steps),
                        remaining_timeout_sec: Some(effective_timeout_sec),
                    },
                },
                debug: std::sync::Arc::new(tokio::sync::RwLock::new(
                    crate::subagent_runtime::SubagentDebugSnapshot::default(),
                )),
                cancelled,
                cancel_notify,
                allow_subagent_tool,
            },
        );

        let built = match built {
            Ok(built) => built,
            Err(error) => {
                let failure = self.failure(
                    SkillCallFailureKind::ChildExecutionFailed,
                    format!("Skill call setup failed: {}", error),
                    true,
                    Some("Retry only if the environment changes; otherwise use a different plan."),
                    json!({
                        "target_skill": &target_skill,
                        "lineage": &child_lineage,
                        "effective_tools": &effective_tools,
                        "effective_max_steps": effective_max_steps,
                        "effective_timeout_sec": effective_timeout_sec,
                    }),
                );
                return self
                    .failure_output(SkillFailureContext {
                        tool_ctx: ctx,
                        skill_name: &target_skill,
                        lineage: child_lineage,
                        effective_tools: effective_tools.clone(),
                        effective_max_steps,
                        effective_timeout_sec,
                        failure,
                        event_type: "skill_call_denied",
                    })
                    .await;
            }
        };

        parent_context
            .total_skill_calls
            .fetch_add(1, Ordering::SeqCst);

        let crate::session::factory::BuiltSubagentSession {
            sub_session_id,
            transcript_path,
            event_log_path,
            mut agent_loop,
            collector,
        } = built;

        self.append_event(
            Some(ctx),
            &ctx.session_id,
            "skill_call_started",
            json!({
                "root_session_id": &parent_context.root_session_id,
                "parent_session_id": &ctx.session_id,
                "sub_session_id": &sub_session_id,
                "target_skill": &target_skill,
                "depth": child_context_preview.current_depth(),
                "lineage": &child_lineage,
                "remaining_steps": effective_max_steps,
                "remaining_timeout_sec": effective_timeout_sec,
                "effective_tools": &effective_tools,
            }),
        )
        .await;

        let span = tracing::info_span!(
            "skill_call_subagent",
            root_session_id = %parent_context.root_session_id,
            parent_session_id = %ctx.session_id,
            sub_session_id = %sub_session_id,
            skill = %target_skill,
            depth = child_context_preview.current_depth(),
            lineage = %child_lineage.join(" -> ")
        );

        let run_result =
            tokio::time::timeout(Duration::from_secs(effective_timeout_sec), async move {
                agent_loop.step(goal).await
            })
            .instrument(span)
            .await;

        let collected_text = collector.take_text().await;
        let tool_outputs = collector.take_tool_outputs().await;
        let artifacts = collector.take_artifacts().await;

        let result = match run_result {
            Ok(Ok(exit)) => {
                let (ok, mut summary, failure) = match exit {
                    crate::core::RunExit::Finished(summary) => (true, summary, None),
                    crate::core::RunExit::EnergyDepleted(summary) => (
                        false,
                        summary.clone(),
                        Some(self.failure(
                            SkillCallFailureKind::BudgetExceeded,
                            "Child skill exhausted its step budget.",
                            false,
                            Some("Do not retry with the same budget. Reduce scope or summarize the partial result."),
                            json!({
                                "target_skill": &target_skill,
                                "lineage": &child_lineage,
                                "effective_max_steps": effective_max_steps,
                            }),
                        )),
                    ),
                    crate::core::RunExit::YieldedToUser => (
                        false,
                        "Child skill yielded control before finishing.".to_string(),
                        Some(self.failure(
                            SkillCallFailureKind::ChildExecutionFailed,
                            "Child skill yielded control before finishing.",
                            false,
                            Some("Continue in the parent skill instead of retrying the same child immediately."),
                            json!({
                                "target_skill": &target_skill,
                                "lineage": &child_lineage,
                                "exit": "yielded_to_user",
                            }),
                        )),
                    ),
                    crate::core::RunExit::StoppedByUser => (
                        false,
                        "Child skill execution was interrupted by user.".to_string(),
                        Some(self.failure(
                            SkillCallFailureKind::ChildExecutionFailed,
                            "Child skill execution was interrupted by user.",
                            true,
                            Some("Only retry if the user explicitly asks to resume this delegation."),
                            json!({
                                "target_skill": &target_skill,
                                "lineage": &child_lineage,
                                "exit": "stopped_by_user",
                            }),
                        )),
                    ),
                    crate::core::RunExit::RecoverableFailed(message)
                    | crate::core::RunExit::AutopilotStalled(message) => (
                        false,
                        message.clone(),
                        Some(self.failure(
                            SkillCallFailureKind::ChildExecutionFailed,
                            message,
                            true,
                            Some("Adjust the plan before retrying the child skill."),
                            json!({
                                "target_skill": &target_skill,
                                "lineage": &child_lineage,
                                "effective_max_steps": effective_max_steps,
                                "effective_timeout_sec": effective_timeout_sec,
                            }),
                        )),
                    ),
                    crate::core::RunExit::CriticallyFailed(message) => (
                        false,
                        message.clone(),
                        Some(self.failure(
                            SkillCallFailureKind::ChildExecutionFailed,
                            message,
                            false,
                            Some("Do not retry this child skill. Re-evaluate the strategy entirely."),
                            json!({
                                "target_skill": &target_skill,
                                "lineage": &child_lineage,
                                "effective_max_steps": effective_max_steps,
                                "effective_timeout_sec": effective_timeout_sec,
                            }),
                        )),
                    ),
                };

                if !collected_text.is_empty() {
                    summary.push_str(&format!("\n\n[Skill Output text]:\n{}", collected_text));
                }

                SkillCallResult {
                    ok,
                    skill_name: target_skill.clone(),
                    summary,
                    findings: tool_outputs,
                    artifacts,
                    lineage: child_lineage.clone(),
                    effective_tools: effective_tools.clone(),
                    effective_max_steps,
                    effective_timeout_sec,
                    sub_session_id: Some(sub_session_id),
                    transcript_path: Some(transcript_path),
                    event_log_path: Some(event_log_path),
                    failure,
                }
            }
            Ok(Err(error)) => {
                SkillCallResult {
                    ok: false,
                    skill_name: target_skill.clone(),
                    summary: format!("Skill error: {}", error),
                    findings: tool_outputs,
                    artifacts,
                    lineage: child_lineage.clone(),
                    effective_tools: effective_tools.clone(),
                    effective_max_steps,
                    effective_timeout_sec,
                    sub_session_id: Some(sub_session_id),
                    transcript_path: Some(transcript_path),
                    event_log_path: Some(event_log_path),
                    failure: Some(self.failure(
                        SkillCallFailureKind::ChildExecutionFailed,
                        format!("Skill error: {}", error),
                        true,
                        Some("Adjust the child request or choose a different skill before retrying."),
                        json!({
                            "target_skill": &target_skill,
                            "lineage": child_context_preview.lineage_names(),
                            "effective_max_steps": effective_max_steps,
                            "effective_timeout_sec": effective_timeout_sec,
                        }),
                    )),
                }
            }
            Err(_) => {
                SkillCallResult {
                    ok: false,
                    skill_name: target_skill.clone(),
                    summary: format!("Skill call timed out after {}s", effective_timeout_sec),
                    findings: tool_outputs,
                    artifacts,
                    lineage: child_context_preview.lineage_names(),
                    effective_tools: effective_tools.clone(),
                    effective_max_steps,
                    effective_timeout_sec,
                    sub_session_id: Some(sub_session_id),
                    transcript_path: Some(transcript_path),
                    event_log_path: Some(event_log_path),
                    failure: Some(self.failure(
                        SkillCallFailureKind::Timeout,
                        format!("Skill call timed out after {}s", effective_timeout_sec),
                        true,
                        Some("Reduce scope, lower tool usage, or summarize partial results instead of retrying immediately."),
                        json!({
                            "target_skill": &target_skill,
                            "lineage": child_context_preview.lineage_names(),
                            "effective_max_steps": effective_max_steps,
                            "effective_timeout_sec": effective_timeout_sec,
                        }),
                    )),
                }
            }
        };

        self.append_event(
            Some(ctx),
            &ctx.session_id,
            "skill_call_finished",
            json!({
                "target_skill": &result.skill_name,
                "lineage": &result.lineage,
                "ok": result.ok,
                "summary": &result.summary,
                "failure": &result.failure,
                "sub_session_id": &result.sub_session_id,
                "effective_tools": &result.effective_tools,
                "effective_max_steps": result.effective_max_steps,
                "effective_timeout_sec": result.effective_timeout_sec,
            }),
        )
        .await;

        StructuredToolOutput::new(
            "call_skill",
            result.ok,
            serde_json::to_string_pretty(&result)
                .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?,
            None,
            None,
            false,
        )
        .to_json_string()
    }
}
