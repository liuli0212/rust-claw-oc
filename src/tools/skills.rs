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
use crate::skills::call_tree::{SkillBudget, SkillCallContext, SkillSessionSeed};
use crate::skills::policy::SkillToolPolicy;
use crate::skills::registry::SkillRegistry;

pub struct CallSkillTool {
    llm: Arc<dyn crate::llm_client::LlmClient>,
    base_tools: Vec<Arc<dyn Tool>>,
    registry: SkillRegistry,
    policy: SkillToolPolicy,
}

const MAX_SKILL_CALL_DEPTH: usize = 3;
const MAX_SKILL_CALLS_PER_ROOT_REQUEST: usize = 6;

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

    async fn append_event(&self, session_id: &str, event_type: &str, payload: serde_json::Value) {
        let _ = EventLog::new(session_id)
            .append(AgentEvent::new(
                event_type,
                session_id.to_string(),
                None,
                None,
                payload,
            ))
            .await;
    }

    fn runtime_allows_nested_tool(name: &str) -> bool {
        !matches!(
            name,
            "dispatch_subagent"
                | "spawn_subagent"
                | "get_subagent_result"
                | "cancel_subagent"
                | "list_subagent_jobs"
        )
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

    #[allow(clippy::too_many_arguments)]
    async fn failure_output(
        &self,
        ctx: &ToolContext,
        skill_name: &str,
        lineage: Vec<String>,
        effective_tools: Vec<String>,
        effective_max_steps: usize,
        effective_timeout_sec: u64,
        failure: SkillCallFailure,
        event_type: &str,
    ) -> Result<String, ToolError> {
        let payload = json!({
            "skill_name": skill_name,
            "lineage": lineage,
            "failure": &failure,
            "effective_tools": effective_tools,
            "effective_max_steps": effective_max_steps,
            "effective_timeout_sec": effective_timeout_sec,
        });
        self.append_event(&ctx.session_id, event_type, payload)
            .await;
        StructuredToolOutput::new(
            "call_skill",
            false,
            serde_json::to_string_pretty(&SkillCallResult {
                ok: false,
                skill_name: skill_name.to_string(),
                summary: failure.message.clone(),
                findings: Vec::new(),
                artifacts: Vec::new(),
                lineage,
                effective_tools,
                effective_max_steps,
                effective_timeout_sec,
                sub_session_id: None,
                transcript_path: None,
                event_log_path: None,
                failure: Some(failure),
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
                allow_subagents: true,
                ..SkillConstraints::default()
            },
        }
    }

    fn make_tool(registry: SkillRegistry) -> CallSkillTool {
        CallSkillTool {
            llm: Arc::new(DummyLlm),
            base_tools: Vec::new(),
            registry,
            policy: SkillToolPolicy::new(),
        }
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
    async fn test_call_skill_requires_active_parent_skill() {
        let tool = make_tool(SkillRegistry::new());
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
        assert!(!payload.ok);
        let failure = payload.failure.unwrap();
        assert!(matches!(failure.kind, SkillCallFailureKind::PolicyDenied));
        assert!(failure.message.contains("requires an active parent skill"));
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
            .store(MAX_SKILL_CALLS_PER_ROOT_REQUEST, Ordering::SeqCst);

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
        let parent_skill = match ctx.active_skill_name.clone() {
            Some(skill_name) => skill_name,
            None => {
                let failure = self.failure(
                    SkillCallFailureKind::PolicyDenied,
                    "Denied skill call: call_skill requires an active parent skill.",
                    false,
                    Some("Activate or continue a parent skill before delegating to another skill."),
                    json!({ "target_skill": &target_skill }),
                );
                return self
                    .failure_output(
                        ctx,
                        &target_skill,
                        Vec::new(),
                        Vec::new(),
                        1,
                        parsed.timeout_sec.unwrap_or(120),
                        failure,
                        "skill_call_denied_policy",
                    )
                    .await;
            }
        };

        let parent_context = ctx.skill_call_context.clone().unwrap_or_else(|| {
            SkillCallContext::new_root(ctx.session_id.clone()).append_frame(&parent_skill, None)
        });
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
            &ctx.session_id,
            "skill_call_requested",
            json!({
                "parent_skill": &parent_skill,
                "target_skill": &target_skill,
                "lineage": &child_lineage,
                "requested_max_steps": requested_steps,
                "requested_timeout_sec": requested_timeout_sec,
                "parent_remaining_steps": parent_remaining_steps,
                "parent_remaining_timeout_sec": parent_remaining_timeout_sec,
            }),
        )
        .await;

        if parent_context.contains_skill(&target_skill) || parent_skill == target_skill {
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
                .failure_output(
                    ctx,
                    &target_skill,
                    child_lineage,
                    Vec::new(),
                    effective_max_steps,
                    effective_timeout_sec,
                    failure,
                    "skill_call_denied_cycle",
                )
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
                .failure_output(
                    ctx,
                    &target_skill,
                    child_lineage,
                    Vec::new(),
                    effective_max_steps,
                    effective_timeout_sec,
                    failure,
                    "skill_call_denied_depth",
                )
                .await;
        }

        if parent_context.total_skill_calls_used() >= MAX_SKILL_CALLS_PER_ROOT_REQUEST {
            let failure = self.failure(
                SkillCallFailureKind::BudgetExceeded,
                format!(
                    "Denied skill call: max total nested skill calls exceeded ({})",
                    MAX_SKILL_CALLS_PER_ROOT_REQUEST
                ),
                false,
                Some("Do not spawn more child skills. Summarize findings or finish with the work already completed."),
                json!({
                    "target_skill": &target_skill,
                    "lineage": &child_lineage,
                    "total_skill_calls_used": parent_context.total_skill_calls_used(),
                    "max_total_skill_calls": MAX_SKILL_CALLS_PER_ROOT_REQUEST,
                }),
            );
            return self
                .failure_output(
                    ctx,
                    &target_skill,
                    child_lineage,
                    Vec::new(),
                    effective_max_steps,
                    effective_timeout_sec,
                    failure,
                    "skill_call_denied_budget",
                )
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
                .failure_output(
                    ctx,
                    &target_skill,
                    child_lineage,
                    Vec::new(),
                    effective_max_steps,
                    effective_timeout_sec,
                    failure,
                    "skill_call_denied_policy",
                )
                .await;
        };

        let parent_visible_tools = self.canonicalize_tools(&ctx.visible_tools);
        let callee_declared_tools = self.canonicalize_tools(&def.meta.allowed_tools);
        let caller_requested_tools = self.canonicalize_tools(&parsed.allowed_tools);
        
        let runtime_allowed_tools: Vec<String> = parent_visible_tools
            .iter()
            .filter(|name| Self::runtime_allows_nested_tool(name.as_str()))
            .cloned()
            .collect();
            
        let effective_tools = if callee_declared_tools.is_empty() {
            if caller_requested_tools.is_empty() {
                runtime_allowed_tools.clone()
            } else {
                caller_requested_tools
                    .into_iter()
                    .filter(|name| {
                        parent_visible_tools.contains(name)
                            && Self::runtime_allows_nested_tool(name.as_str())
                    })
                    .collect()
            }
        } else {
            let base = callee_declared_tools
                .iter()
                .filter(|name| {
                    parent_visible_tools.contains(name)
                        && Self::runtime_allows_nested_tool(name.as_str())
                })
                .cloned()
                .collect::<Vec<_>>();
            if caller_requested_tools.is_empty() {
                base
            } else {
                base.into_iter()
                    .filter(|name| caller_requested_tools.contains(name))
                    .collect()
            }
        };
        let missing_tools: Vec<String> = callee_declared_tools
            .iter()
            .filter(|name| {
                !parent_visible_tools.contains(name)
                    || !Self::runtime_allows_nested_tool(name.as_str())
            })
            .cloned()
            .collect();

        if !missing_tools.is_empty() {
            let failure = self.failure(
                SkillCallFailureKind::MissingTools,
                format!(
                    "Denied skill call: child requires tools missing in parent context: [{}]",
                    missing_tools.join(", ")
                ),
                false,
                Some("Do not retry this child skill with the same parent context. Choose a different skill or continue without those tools."),
                json!({
                    "target_skill": &target_skill,
                    "lineage": &child_lineage,
                    "missing_tools": &missing_tools,
                    "parent_visible_tools": &parent_visible_tools,
                }),
            );
            return self
                .failure_output(
                    ctx,
                    &target_skill,
                    child_lineage,
                    effective_tools,
                    effective_max_steps,
                    effective_timeout_sec,
                    failure,
                    "skill_call_denied_policy",
                )
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
        let session_allowed_tools = if effective_tools.is_empty() {
            vec!["__skill_no_tools__".to_string()]
        } else {
            effective_tools.clone()
        };

        let built = crate::session::factory::build_subagent_session(
            ctx,
            self.llm.clone(),
            &self.base_tools,
            crate::session::factory::SubagentBuildMode::SyncCompatible,
            None,
            &session_allowed_tools,
            effective_max_steps,
            effective_timeout_sec,
            &input_summary,
            SkillSessionSeed {
                inherited_call_context: Some(parent_context.clone()),
                inherited_budget: SkillBudget {
                    remaining_steps: Some(effective_max_steps),
                    remaining_timeout_sec: Some(effective_timeout_sec),
                },
            },
            std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::subagent_runtime::SubagentDebugSnapshot::default(),
            )),
            cancelled,
            cancel_notify,
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
                    .failure_output(
                        ctx,
                        &target_skill,
                        child_lineage,
                        effective_tools,
                        effective_max_steps,
                        effective_timeout_sec,
                        failure,
                        "skill_call_finished",
                    )
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
            rejected_tools: _,
        } = built;

        self.append_event(
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
            lineage = %child_context_preview.lineage_names().join(" -> ")
        );

        let run_result =
            tokio::time::timeout(Duration::from_secs(effective_timeout_sec), async move {
                agent_loop.step(goal).await
            })
            .instrument(span)
            .await;

        let _collected_text = collector.take_text().await;
        let tool_outputs = collector.take_tool_outputs().await;
        let artifacts = collector.take_artifacts().await;

        let result = match run_result {
            Ok(Ok(exit)) => {
                let (ok, summary, failure) = match exit {
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
                                "lineage": child_context_preview.lineage_names(),
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
                                "lineage": child_context_preview.lineage_names(),
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
                                "lineage": child_context_preview.lineage_names(),
                                "exit": "stopped_by_user",
                            }),
                        )),
                    ),
                    crate::core::RunExit::RecoverableFailed(message)
                    | crate::core::RunExit::CriticallyFailed(message)
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
                                "lineage": child_context_preview.lineage_names(),
                                "effective_max_steps": effective_max_steps,
                                "effective_timeout_sec": effective_timeout_sec,
                            }),
                        )),
                    ),
                };

                SkillCallResult {
                    ok,
                    skill_name: target_skill.clone(),
                    summary,
                    findings: tool_outputs,
                    artifacts,
                    lineage: child_context_preview.lineage_names(),
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
                    lineage: child_context_preview.lineage_names(),
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
