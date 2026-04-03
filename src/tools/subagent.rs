use std::collections::BTreeSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::Instrument;

use super::protocol::{clean_schema, StructuredToolOutput, Tool, ToolError};
use crate::skills::call_tree::{
    SkillBudget, SkillSessionSeed, MAX_DELEGATION_CALLS_PER_ROOT_REQUEST,
};
use crate::subagent_runtime::{
    SubagentRuntime, DEFAULT_SUBAGENT_MAX_STEPS, DEFAULT_SUBAGENT_TIMEOUT_SEC,
};

/// The unified subagent arguments.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SubagentArgs {
    Run {
        /// The concrete task the subagent should complete.
        goal: String,
        /// Optional parent context the subagent can rely on.
        #[serde(default, alias = "input_summary")]
        context: String,
        /// If true, spawn the subagent as a background job.
        #[serde(default, alias = "run_in_background")]
        background: bool,
    },
    Status {
        job_id: String,
        /// Optional long-poll timeout in seconds.
        #[serde(default)]
        wait_sec: Option<u64>,
        /// If true, mark a terminal result as consumed.
        #[serde(default)]
        consume: bool,
    },
    Cancel {
        job_id: String,
    },
    List,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentResult {
    pub ok: bool,
    pub summary: String,
    pub findings: Vec<String>,
    pub artifacts: Vec<String>,
    pub sub_session_id: Option<String>,
    pub transcript_path: Option<String>,
    pub event_log_path: Option<String>,
}

pub struct SubagentTool {
    llm: Arc<dyn crate::llm_client::LlmClient>,
    base_tools: Vec<Arc<dyn Tool>>,
    runtime: SubagentRuntime,
}

impl SubagentTool {
    pub fn new(
        llm: Arc<dyn crate::llm_client::LlmClient>,
        base_tools: Vec<Arc<dyn Tool>>,
        runtime: SubagentRuntime,
    ) -> Self {
        Self {
            llm,
            base_tools,
            runtime,
        }
    }

    fn serialize_output(tool_name: &str, payload: Value) -> Result<String, ToolError> {
        StructuredToolOutput::new(
            tool_name,
            true,
            serde_json::to_string_pretty(&payload)
                .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?,
            Some(0),
            None,
            false,
        )
        .to_json_string()
    }

    fn validate_args_shape(args: &Value) -> Result<(), ToolError> {
        let Some(obj) = args.as_object() else {
            return Err(ToolError::InvalidArguments(
                "subagent expects a JSON object payload".to_string(),
            ));
        };

        let Some(action) = obj.get("action").and_then(Value::as_str) else {
            return Err(ToolError::InvalidArguments(
                "subagent requires an `action` field".to_string(),
            ));
        };

        let allowed_keys: &[&str] = match action {
            "run" => &[
                "action",
                "goal",
                "context",
                "input_summary",
                "background",
                "run_in_background",
            ],
            "status" => &["action", "job_id", "wait_sec", "consume"],
            "cancel" => &["action", "job_id"],
            "list" => &["action"],
            other => {
                return Err(ToolError::InvalidArguments(format!(
                    "Unknown subagent action: {}",
                    other
                )));
            }
        };

        let unknown: BTreeSet<String> = obj
            .keys()
            .filter(|key| !allowed_keys.contains(&key.as_str()))
            .cloned()
            .collect();

        if unknown.is_empty() {
            return Ok(());
        }

        Err(ToolError::InvalidArguments(format!(
            "Unknown field(s) for subagent action `{}`: {}",
            action,
            unknown.into_iter().collect::<Vec<_>>().join(", ")
        )))
    }

    fn effective_limits(ctx: &super::protocol::ToolContext) -> (usize, u64) {
        let max_steps = ctx
            .skill_budget
            .remaining_steps
            .unwrap_or(DEFAULT_SUBAGENT_MAX_STEPS)
            .clamp(1, DEFAULT_SUBAGENT_MAX_STEPS);
        let timeout_sec = ctx
            .skill_budget
            .remaining_timeout_sec
            .unwrap_or(DEFAULT_SUBAGENT_TIMEOUT_SEC)
            .clamp(1, DEFAULT_SUBAGENT_TIMEOUT_SEC);
        (max_steps, timeout_sec)
    }

    fn inherited_skill_session_seed(
        ctx: &super::protocol::ToolContext,
        max_steps: usize,
        timeout_sec: u64,
    ) -> SkillSessionSeed {
        SkillSessionSeed {
            inherited_call_context: ctx.skill_call_context.clone(),
            inherited_budget: SkillBudget {
                remaining_steps: Some(max_steps),
                remaining_timeout_sec: Some(timeout_sec),
            },
        }
    }

    fn validate_delegation_budget(
        ctx: &super::protocol::ToolContext,
    ) -> Result<Option<crate::skills::call_tree::SkillCallContext>, ToolError> {
        let Some(call_context) = ctx.skill_call_context.clone() else {
            return Ok(None);
        };

        let used = call_context.total_skill_calls_used();
        if used >= MAX_DELEGATION_CALLS_PER_ROOT_REQUEST {
            return Err(ToolError::ExecutionFailed(format!(
                "Nested delegation budget exceeded ({}). Finish existing skill work before spawning more delegated subagents.",
                MAX_DELEGATION_CALLS_PER_ROOT_REQUEST
            )));
        }

        Ok(Some(call_context))
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> String {
        "subagent".to_string()
    }

    fn description(&self) -> String {
        "Manage delegated subagents. Use `action=\"run\"` with `goal` and optional `context` to run immediately, or set `background=true` to spawn a background job. \
         Use `action=\"status\"` with optional `wait_sec` to inspect or wait for a job, `action=\"cancel\"` to abort, and `action=\"list\"` to enumerate jobs."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        clean_schema(serde_json::to_value(schema_for!(SubagentArgs)).unwrap())
    }

    fn has_side_effects(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &super::protocol::ToolContext,
    ) -> Result<String, ToolError> {
        tracing::debug!("SubagentTool invoked within session: {}", ctx.session_id);
        Self::validate_args_shape(&args)?;
        let parsed: SubagentArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        match parsed {
            SubagentArgs::Run {
                goal,
                context,
                background,
            } => {
                let (max_steps, timeout_sec) = Self::effective_limits(ctx);
                let inherited_call_context = Self::validate_delegation_budget(ctx)?;
                let skill_session_seed = SkillSessionSeed {
                    inherited_call_context,
                    ..Self::inherited_skill_session_seed(ctx, max_steps, timeout_sec)
                };

                if background {
                    let spawned = self
                        .runtime
                        .spawn_job_with_limits(
                            ctx.clone(),
                            goal.clone(),
                            context.clone(),
                            timeout_sec,
                            max_steps,
                            skill_session_seed,
                        )
                        .await?;
                    if let Some(call_context) = ctx.skill_call_context.as_ref() {
                        call_context
                            .total_skill_calls
                            .fetch_add(1, Ordering::SeqCst);
                    }
                    Self::serialize_output(
                        "subagent",
                        json!({
                            "job_id": spawned.job_id,
                            "sub_session_id": spawned.sub_session_id,
                            "status": "spawned",
                        }),
                    )
                } else {
                    tracing::info!(
                        "Dispatching sync subagent with goal: '{}', timeout: {}s, max_steps: {}",
                        goal,
                        timeout_sec,
                        max_steps
                    );

                    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let cancel_notify = Arc::new(tokio::sync::Notify::new());
                    let built = crate::session::factory::build_subagent_session(
                        ctx,
                        self.llm.clone(),
                        &self.base_tools,
                        crate::session::factory::SubagentSessionConfig {
                            sub_session_id: None,
                            allowed_tools: Vec::new(),
                            energy_budget: max_steps,
                            timeout_sec,
                            parent_context_text: context.clone(),
                            skill_session_seed,
                            debug: std::sync::Arc::new(tokio::sync::RwLock::new(
                                crate::subagent_runtime::SubagentDebugSnapshot::default(),
                            )),
                            cancelled,
                            cancel_notify,
                            allow_subagent_tool: false,
                        },
                    )
                    .map_err(ToolError::ExecutionFailed)?;
                    if let Some(call_context) = ctx.skill_call_context.as_ref() {
                        call_context
                            .total_skill_calls
                            .fetch_add(1, Ordering::SeqCst);
                    }

                    let crate::session::factory::BuiltSubagentSession {
                        sub_session_id,
                        transcript_path,
                        event_log_path,
                        mut agent_loop,
                        collector,
                    } = built;
                    let span = tracing::info_span!(
                        "subagent_run_sync",
                        parent_session_id = %ctx.session_id,
                        sub_session_id = %sub_session_id,
                        goal = %goal
                    );

                    let goal_for_step = goal.clone();
                    let run_result =
                        tokio::time::timeout(Duration::from_secs(timeout_sec), async move {
                            agent_loop.step(goal_for_step).await
                        })
                        .instrument(span)
                        .await;

                    tracing::info!("Subagent execution completed.");

                    let collected_text = collector.take_text().await;
                    let tool_outputs = collector.take_tool_outputs().await;
                    let artifacts = collector.take_artifacts().await;

                    let result = match run_result {
                        Ok(Ok(exit)) => {
                            let ok = matches!(exit, crate::core::RunExit::Finished(_));
                            let summary = match exit {
                                crate::core::RunExit::Finished(summary) => summary,
                                crate::core::RunExit::YieldedToUser => {
                                    if collected_text.trim().is_empty() {
                                        "Sub-agent yielded without visible output.".to_string()
                                    } else {
                                        format!(
                                            "Sub-agent yielded with output: {}",
                                            collected_text.trim()
                                        )
                                    }
                                }
                                crate::core::RunExit::RecoverableFailed(message)
                                | crate::core::RunExit::CriticallyFailed(message)
                                | crate::core::RunExit::AutopilotStalled(message) => message,
                                crate::core::RunExit::EnergyDepleted(summary) => summary,
                                crate::core::RunExit::StoppedByUser => {
                                    "Sub-agent execution was interrupted.".to_string()
                                }
                            };

                            let status_label = if ok { "Finished" } else { "Failed" };
                            tracing::info!(
                                target: "subagent",
                                "[Sub:sync] {} with summary: {}",
                                status_label,
                                summary
                            );

                            SubagentResult {
                                ok,
                                summary,
                                findings: tool_outputs,
                                artifacts,
                                sub_session_id: Some(sub_session_id),
                                transcript_path: Some(transcript_path),
                                event_log_path: Some(event_log_path),
                            }
                        }
                        Ok(Err(error)) => {
                            tracing::warn!("Subagent encountered an error: {}", error);
                            SubagentResult {
                                ok: false,
                                summary: format!("Sub-agent error: {}", error),
                                findings: tool_outputs,
                                artifacts,
                                sub_session_id: Some(sub_session_id),
                                transcript_path: Some(transcript_path),
                                event_log_path: Some(event_log_path),
                            }
                        }
                        Err(_) => {
                            tracing::warn!("Subagent timed out after {}s", timeout_sec);
                            SubagentResult {
                                ok: false,
                                summary: format!(
                                    "Sub-agent timed out after {}s while working on '{}'.",
                                    timeout_sec, goal
                                ),
                                findings: tool_outputs,
                                artifacts,
                                sub_session_id: Some(sub_session_id),
                                transcript_path: Some(transcript_path),
                                event_log_path: Some(event_log_path),
                            }
                        }
                    };

                    StructuredToolOutput::new(
                        "subagent",
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
            SubagentArgs::Status {
                job_id,
                wait_sec,
                consume,
            } => {
                if let Some(wait_sec) = wait_sec {
                    if let Some(handle) = self.runtime.get_job_handle(&job_id).await {
                        let deadline =
                            tokio::time::Instant::now() + std::time::Duration::from_secs(wait_sec);
                        loop {
                            let state = handle.state.read().await;
                            if state.is_terminal() {
                                break;
                            }
                            drop(state);

                            let remaining =
                                deadline.saturating_duration_since(tokio::time::Instant::now());
                            if remaining.is_zero() {
                                break;
                            }

                            tokio::select! {
                                _ = handle.completion_notify.notified() => {}
                                _ = tokio::time::sleep(remaining.min(std::time::Duration::from_secs(2))) => {}
                            }
                        }
                    }
                }

                let snapshot = self.runtime.get_job_snapshot(&job_id, consume).await?;

                if snapshot.state.is_terminal() {
                    let status = snapshot.state.finish_reason();
                    let summary_text = match &snapshot.state {
                        crate::subagent_runtime::SubagentJobState::Completed { result, .. } => {
                            result.summary.clone()
                        }
                        crate::subagent_runtime::SubagentJobState::Failed {
                            error,
                            partial,
                            ..
                        } => partial
                            .as_ref()
                            .map(|p| p.summary.clone())
                            .unwrap_or_else(|| error.clone()),
                        crate::subagent_runtime::SubagentJobState::Cancelled {
                            partial, ..
                        } => partial
                            .as_ref()
                            .map(|p| p.summary.clone())
                            .unwrap_or_else(|| "Cancelled".to_string()),
                        crate::subagent_runtime::SubagentJobState::TimedOut { partial, .. } => {
                            partial
                                .as_ref()
                                .map(|p| p.summary.clone())
                                .unwrap_or_else(|| "Timed out".to_string())
                        }
                        _ => String::new(),
                    };
                    tracing::info!(
                        target: "subagent",
                        "[Sub:{}] Fetched {} result: {}",
                        job_id,
                        status,
                        summary_text
                    );
                }

                Self::serialize_output(
                    "subagent",
                    json!({
                        "job_id": snapshot.meta.job_id,
                        "status": snapshot.state.finish_reason(),
                        "consumed": snapshot.consumed,
                        "consumed_at_unix_ms": snapshot.consumed_at_unix_ms,
                        "debug": snapshot.debug,
                        "state": snapshot.state,
                    }),
                )
            }
            SubagentArgs::Cancel { job_id } => {
                self.runtime.cancel_job(&job_id).await?;
                Self::serialize_output(
                    "subagent",
                    json!({
                        "job_id": job_id,
                        "status": "cancelling",
                    }),
                )
            }
            SubagentArgs::List => {
                let jobs = self.runtime.list_jobs().await;
                Self::serialize_output(
                    "subagent",
                    json!({
                        "jobs": jobs,
                    }),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::mpsc;

    use crate::context::{FunctionCall, Message};
    use crate::llm_client::{LlmClient, LlmError, StreamEvent};
    use crate::skills::call_tree::{SkillBudget, SkillCallContext};
    use crate::tools::protocol::{ToolContext, ToolExecutionEnvelope};

    fn make_ctx() -> ToolContext {
        ToolContext::new("parent", "cli")
    }

    fn make_skill_ctx(
        used_calls: usize,
        remaining_steps: usize,
        remaining_timeout_sec: u64,
    ) -> ToolContext {
        let mut ctx = make_ctx();
        let call_context = SkillCallContext::new_root("root").append_frame("planner", None);
        call_context
            .total_skill_calls
            .store(used_calls, Ordering::SeqCst);
        ctx.skill_call_context = Some(call_context);
        ctx.skill_budget = SkillBudget {
            remaining_steps: Some(remaining_steps),
            remaining_timeout_sec: Some(remaining_timeout_sec),
        };
        ctx
    }

    fn parse_payload(result: &str) -> Value {
        let envelope = ToolExecutionEnvelope::from_json_str(result).expect("valid tool envelope");
        serde_json::from_str(&envelope.result.output).expect("valid subagent payload")
    }

    struct FinishImmediatelyLlm;

    #[async_trait]
    impl LlmClient for FinishImmediatelyLlm {
        fn model_name(&self) -> &str {
            "finish-immediately"
        }

        fn provider_name(&self) -> &str {
            "test"
        }

        async fn stream(
            &self,
            _messages: Vec<Message>,
            _system_instruction: Option<Message>,
            _tools: Vec<Arc<dyn Tool>>,
        ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
            let (tx, rx) = mpsc::channel(4);
            let _ = tx.try_send(StreamEvent::ToolCall(
                FunctionCall {
                    name: "finish_task".to_string(),
                    args: json!({ "summary": "done" }),
                    id: Some("tc_1".to_string()),
                },
                None,
            ));
            let _ = tx.try_send(StreamEvent::Done);
            Ok(rx)
        }
    }

    struct HangingLlm;

    #[async_trait]
    impl LlmClient for HangingLlm {
        fn model_name(&self) -> &str {
            "hanging"
        }

        fn provider_name(&self) -> &str {
            "test"
        }

        async fn stream(
            &self,
            _messages: Vec<Message>,
            _system_instruction: Option<Message>,
            _tools: Vec<Arc<dyn Tool>>,
        ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
            let (tx, rx) = mpsc::channel(4);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(60)).await;
                drop(tx);
            });
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

    fn make_tool() -> SubagentTool {
        let llm: Arc<dyn LlmClient> = Arc::new(FinishImmediatelyLlm);
        let base_tools: Vec<Arc<dyn Tool>> = vec![Arc::new(MockTool("read_file"))];
        let runtime = SubagentRuntime::new(llm.clone(), base_tools.clone(), 2);
        SubagentTool::new(llm, base_tools, runtime)
    }

    fn make_tool_with_llm(llm: Arc<dyn LlmClient>) -> SubagentTool {
        let base_tools: Vec<Arc<dyn Tool>> = vec![Arc::new(MockTool("read_file"))];
        let runtime = SubagentRuntime::new(llm.clone(), base_tools.clone(), 2);
        SubagentTool::new(llm, base_tools, runtime)
    }

    #[tokio::test]
    async fn test_subagent_rejects_unknown_run_fields() {
        let tool = make_tool();
        let err = tool
            .execute(
                json!({
                    "action": "run",
                    "goal": "inspect",
                    "input_summary": "summary",
                    "unexpected": true
                }),
                &make_ctx(),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::InvalidArguments(_)));
        assert!(err.to_string().contains("Unknown field(s)"));
    }

    #[tokio::test]
    async fn test_subagent_rejects_deprecated_execution_knobs() {
        let tool = make_tool();
        let err = tool
            .execute(
                json!({
                    "action": "run",
                    "goal": "edit parser",
                    "input_summary": "repo context",
                    "allow_writes": true,
                    "claimed_paths": ["src/parser.rs"],
                    "allowed_tools": ["read_file"],
                    "timeout_sec": 5,
                    "max_steps": 4
                }),
                &make_ctx(),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::InvalidArguments(_)));
        assert!(err.to_string().contains("Unknown field(s)"));
    }

    #[tokio::test]
    async fn test_subagent_run_accepts_legacy_aliases() {
        let tool = make_tool();
        let result = tool
            .execute(
                json!({
                    "action": "run",
                    "goal": "inspect",
                    "context": "legacy summary",
                    "background": true
                }),
                &make_ctx(),
            )
            .await
            .unwrap();

        let payload = parse_payload(&result);
        assert_eq!(payload["status"], Value::String("spawned".to_string()));
        assert!(payload["job_id"].as_str().is_some());
    }

    #[tokio::test]
    async fn test_subagent_background_spawn_and_status_can_consume_result() {
        let tool = make_tool();
        let spawned = tool
            .execute(
                json!({
                    "action": "run",
                    "goal": "inspect",
                    "input_summary": "summary",
                    "run_in_background": true
                }),
                &make_ctx(),
            )
            .await
            .unwrap();
        let spawned_payload = parse_payload(&spawned);
        let job_id = spawned_payload["job_id"].as_str().unwrap().to_string();

        let mut consumed_at = None;
        for _ in 0..400 {
            let result = tool
                .execute(
                    json!({
                        "action": "status",
                        "job_id": job_id.clone(),
                        "consume": true
                    }),
                    &make_ctx(),
                )
                .await
                .unwrap();
            let payload = parse_payload(&result);
            if payload["status"] == Value::String("finished".to_string()) {
                assert_eq!(payload["consumed"], Value::Bool(true));
                consumed_at = payload["consumed_at_unix_ms"].as_u64();
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        assert!(consumed_at.is_some());
    }

    #[tokio::test]
    async fn test_subagent_sync_run_respects_inherited_timeout_budget() {
        let tool = make_tool_with_llm(Arc::new(HangingLlm));
        let ctx = make_skill_ctx(0, 4, 1);

        let result = tool
            .execute(
                json!({
                    "action": "run",
                    "goal": "inspect",
                    "context": "summary"
                }),
                &ctx,
            )
            .await
            .unwrap();

        let payload = parse_payload(&result);
        assert_eq!(payload["ok"], Value::Bool(false));
        assert!(payload["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("timed out after 1s"));
    }

    #[tokio::test]
    async fn test_subagent_run_consumes_shared_delegation_budget() {
        let tool = make_tool();
        let ctx = make_skill_ctx(MAX_DELEGATION_CALLS_PER_ROOT_REQUEST - 1, 4, 10);

        let result = tool
            .execute(
                json!({
                    "action": "run",
                    "goal": "inspect",
                    "context": "summary"
                }),
                &ctx,
            )
            .await
            .unwrap();

        let payload = parse_payload(&result);
        assert_eq!(payload["ok"], Value::Bool(true));
        assert_eq!(
            ctx.skill_call_context
                .as_ref()
                .unwrap()
                .total_skill_calls_used(),
            MAX_DELEGATION_CALLS_PER_ROOT_REQUEST
        );

        let err = tool
            .execute(
                json!({
                    "action": "run",
                    "goal": "inspect again",
                    "context": "summary"
                }),
                &ctx,
            )
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::ExecutionFailed(_)));
        assert!(err
            .to_string()
            .contains("Nested delegation budget exceeded"));
    }

    #[tokio::test]
    async fn test_subagent_cancel_unknown_job_returns_error() {
        let tool = make_tool();
        let err = tool
            .execute(
                json!({
                    "action": "cancel",
                    "job_id": "missing"
                }),
                &make_ctx(),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::ExecutionFailed(_)));
    }
}
