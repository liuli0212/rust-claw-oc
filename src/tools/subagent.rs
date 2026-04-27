use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::protocol::{StructuredToolOutput, Tool, ToolError, clean_schema};
use crate::delegation::{
    DelegationBudget, DelegationSessionSeed, MAX_DELEGATION_CALLS_PER_ROOT_REQUEST,
    SkillDelegationRequest, effective_limits, resolve_skill_delegation,
};
use crate::skills::arguments::validate_json_args;
use crate::skills::policy::SkillToolPolicy;
use crate::skills::registry::SkillRegistry;
use crate::subagent_runtime::{
    DEFAULT_SUBAGENT_MAX_STEPS, DEFAULT_SUBAGENT_TIMEOUT_SEC, SubagentExecutionOrigin,
    SubagentExecutionRequest, SubagentRuntime, SubagentSkillOrigin,
};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SubagentArgs {
    Run {
        /// The concrete task the subagent should complete.
        #[serde(default)]
        goal: Option<String>,
        /// Optional parent context the subagent can rely on.
        #[serde(default, alias = "input_summary")]
        context: String,
        /// If true, spawn the subagent as a background job.
        #[serde(default, alias = "run_in_background")]
        background: bool,
        /// Optional delegated skill name. Mutually exclusive with `goal`.
        #[serde(default)]
        skill_name: Option<String>,
        /// Structured arguments for delegated skill execution.
        #[serde(default)]
        skill_args: Option<Value>,
        /// Optional timeout in seconds.
        #[serde(default)]
        timeout_sec: Option<u64>,
        /// Optional maximum steps.
        #[serde(default)]
        max_steps: Option<usize>,
    },
    Status {
        job_id: String,
        #[serde(default)]
        wait_sec: Option<u64>,
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
    pub skill_name: Option<String>,
    pub lineage: Option<Vec<String>>,
    pub effective_tools: Option<Vec<String>>,
    pub effective_max_steps: Option<usize>,
    pub effective_timeout_sec: Option<u64>,
}

pub struct SubagentTool {
    runtime: SubagentRuntime,
    registry: SkillRegistry,
    policy: SkillToolPolicy,
}

impl SubagentTool {
    pub fn new(runtime: SubagentRuntime) -> Self {
        let mut registry = SkillRegistry::new();
        registry.discover(std::path::Path::new("skills"));
        Self {
            runtime,
            registry,
            policy: SkillToolPolicy::new(),
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
                "skill_name",
                "skill_args",
                "timeout_sec",
                "max_steps",
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

    fn validate_nested_budget(&self, ctx: &super::protocol::ToolContext) -> Result<(), ToolError> {
        let Some(call_context) = ctx.delegation_context.as_ref() else {
            return Ok(());
        };

        if call_context.total_delegations_used() >= MAX_DELEGATION_CALLS_PER_ROOT_REQUEST {
            return Err(ToolError::ExecutionFailed(format!(
                "Nested delegation budget exceeded ({}). Finish existing delegated work before spawning more subagents.",
                MAX_DELEGATION_CALLS_PER_ROOT_REQUEST
            )));
        }

        Ok(())
    }

    fn make_raw_execution_request(
        &self,
        ctx: &super::protocol::ToolContext,
        goal: String,
        context: String,
        requested_timeout_sec: Option<u64>,
        requested_max_steps: Option<usize>,
    ) -> Result<SubagentExecutionRequest, ToolError> {
        self.validate_nested_budget(ctx)?;
        let (effective_max_steps, effective_timeout_sec) = effective_limits(
            &ctx.delegation_budget,
            requested_max_steps,
            requested_timeout_sec,
            DEFAULT_SUBAGENT_MAX_STEPS,
            DEFAULT_SUBAGENT_TIMEOUT_SEC,
        );

        Ok(SubagentExecutionRequest {
            initial_input: goal.clone(),
            display_goal: goal,
            context,
            timeout_sec: effective_timeout_sec,
            max_steps: effective_max_steps,
            allowed_tools: Vec::new(),
            restrict_to_allowed_tools: false,
            allow_subagent_tool: false,
            origin: SubagentExecutionOrigin::Goal,
            effective_max_steps: Some(effective_max_steps),
            effective_timeout_sec: Some(effective_timeout_sec),
            delegation_seed: DelegationSessionSeed {
                inherited_context: ctx.delegation_context.clone(),
                inherited_budget: DelegationBudget {
                    remaining_steps: Some(effective_max_steps),
                    remaining_timeout_sec: Some(effective_timeout_sec),
                },
                delegated_context: None,
            },
        })
    }

    fn make_skill_execution_request(
        &self,
        ctx: &super::protocol::ToolContext,
        skill_name: String,
        skill_args: Option<Value>,
        context: String,
        requested_timeout_sec: Option<u64>,
        requested_max_steps: Option<usize>,
    ) -> Result<SubagentExecutionRequest, ToolError> {
        let resolved = resolve_skill_delegation(
            &self.registry,
            &self.policy,
            ctx,
            &self.runtime.base_tool_names(),
            SkillDelegationRequest {
                skill_name,
                raw_args: None,
                json_args: skill_args.clone(),
                context: context.clone(),
                requested_timeout_sec,
                requested_max_steps,
            },
            DEFAULT_SUBAGENT_MAX_STEPS,
            DEFAULT_SUBAGENT_TIMEOUT_SEC,
        )
        .map_err(|failure| ToolError::ExecutionFailed(failure.message.clone()))?;

        let args_to_validate = skill_args.unwrap_or_else(|| json!({}));
        validate_json_args(resolved.skill.parameters.as_ref(), &args_to_validate)
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;

        Ok(SubagentExecutionRequest {
            initial_input: resolved.activation_command,
            display_goal: resolved.display_goal,
            context,
            timeout_sec: resolved.effective_timeout_sec,
            max_steps: resolved.effective_max_steps,
            allowed_tools: resolved.effective_tools.clone(),
            restrict_to_allowed_tools: true,
            allow_subagent_tool: resolved.allow_subagent_tool,
            origin: SubagentExecutionOrigin::Skill(SubagentSkillOrigin {
                name: resolved.skill.meta.name.clone(),
                lineage: resolved.lineage,
                effective_tools: resolved.effective_tools,
            }),
            effective_max_steps: Some(resolved.effective_max_steps),
            effective_timeout_sec: Some(resolved.effective_timeout_sec),
            delegation_seed: resolved.delegation_seed,
        })
    }

    fn register_delegation_use(&self, ctx: &super::protocol::ToolContext) {
        if let Some(call_context) = ctx.delegation_context.as_ref() {
            call_context
                .total_delegations
                .fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> String {
        "subagent".to_string()
    }

    fn description(&self) -> String {
        "Manage delegated subagents. Use `action=\"run\"` with either `goal` for a normal task or `skill_name` plus optional `skill_args` for a delegated skill. \
         Optional `context`, `timeout_sec`, and `max_steps` are supported. Set `background=true` to spawn a background job. \
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
                skill_name,
                skill_args,
                timeout_sec,
                max_steps,
            } => {
                let request = match (goal, skill_name) {
                    (Some(goal), None) => {
                        self.make_raw_execution_request(ctx, goal, context, timeout_sec, max_steps)?
                    }
                    (None, Some(skill_name)) => self.make_skill_execution_request(
                        ctx,
                        skill_name,
                        skill_args,
                        context,
                        timeout_sec,
                        max_steps,
                    )?,
                    (Some(_), Some(_)) => {
                        return Err(ToolError::InvalidArguments(
                            "`goal` and `skill_name` are mutually exclusive".to_string(),
                        ));
                    }
                    (None, None) => {
                        return Err(ToolError::InvalidArguments(
                            "subagent(action=\"run\") requires either `goal` or `skill_name`"
                                .to_string(),
                        ));
                    }
                };
                self.register_delegation_use(ctx);

                if background {
                    let spawned = self
                        .runtime
                        .spawn_job_with_limits(ctx.clone(), request.clone())
                        .await?;
                    Self::serialize_output(
                        "subagent",
                        json!({
                            "job_id": spawned.job_id,
                            "sub_session_id": spawned.sub_session_id,
                            "status": "spawned",
                            "skill_name": request.skill_name(),
                        }),
                    )
                } else {
                    let result = self.runtime.run_sync(ctx, request).await?;
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
                    self.runtime.wait_for_terminal(&job_id, wait_sec).await;
                }

                let snapshot = self.runtime.get_job_snapshot(&job_id, consume).await?;

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

    use std::sync::Arc;
    use std::time::Duration;

    use serial_test::serial;
    use tokio::sync::mpsc;

    use crate::context::{FunctionCall, Message};
    use crate::delegation::DelegationContext;
    use crate::llm_client::{LlmClient, LlmError, StreamEvent};
    use crate::schema::StoragePaths;
    use crate::tools::protocol::{ToolContext, ToolExecutionEnvelope};
    use crate::trace::{RecordQuery, RunSummary, find_run_for_subsession, get_records, get_run};

    fn make_ctx() -> ToolContext {
        ToolContext::new("parent", "cli")
    }

    fn make_skill_ctx(
        used_calls: usize,
        remaining_steps: usize,
        remaining_timeout_sec: u64,
    ) -> ToolContext {
        let mut ctx = make_ctx();
        let call_context = DelegationContext::new_root("root").append_frame("planner", None);
        call_context
            .total_delegations
            .store(used_calls, Ordering::SeqCst);
        ctx.delegation_context = Some(call_context);
        ctx.delegation_budget = DelegationBudget {
            remaining_steps: Some(remaining_steps),
            remaining_timeout_sec: Some(remaining_timeout_sec),
        };
        ctx
    }

    fn parse_payload(result: &str) -> Value {
        let envelope = ToolExecutionEnvelope::from_json_str(result).expect("valid tool envelope");
        serde_json::from_str(&envelope.result.output).expect("valid subagent payload")
    }

    fn unique_run_id(prefix: &str) -> String {
        format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
    }

    fn write_trace_summary(summary: &RunSummary) {
        let path = StoragePaths::trace_run_summary_file(&summary.run_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, serde_json::to_string(summary).unwrap()).unwrap();
    }

    fn cleanup_trace_run(run_id: &str) {
        let _ = std::fs::remove_file(StoragePaths::trace_run_summary_file(run_id));
        let _ = std::fs::remove_file(StoragePaths::trace_run_records_file(run_id));
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

        fn capabilities(&self) -> crate::llm_client::LlmCapabilities {
            crate::llm_client::LlmCapabilities {
                function_tools: true,
                custom_tools: false,
                parallel_tool_calls: true,
                supports_code_mode: true,
            }
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

        fn capabilities(&self) -> crate::llm_client::LlmCapabilities {
            crate::llm_client::LlmCapabilities {
                function_tools: true,
                custom_tools: false,
                parallel_tool_calls: true,
                supports_code_mode: true,
            }
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
        let runtime = SubagentRuntime::new(llm, base_tools, 2);
        SubagentTool::new(runtime)
    }

    fn make_tool_with_llm(llm: Arc<dyn LlmClient>) -> SubagentTool {
        let base_tools: Vec<Arc<dyn Tool>> = vec![Arc::new(MockTool("read_file"))];
        let runtime = SubagentRuntime::new(llm, base_tools, 2);
        SubagentTool::new(runtime)
    }

    fn make_tool_with_base_tools(base_tools: Vec<Arc<dyn Tool>>) -> SubagentTool {
        let llm: Arc<dyn LlmClient> = Arc::new(FinishImmediatelyLlm);
        let runtime = SubagentRuntime::new(llm, base_tools, 2);
        SubagentTool::new(runtime)
    }

    #[tokio::test]
    async fn test_subagent_rejects_unknown_run_fields() {
        let tool = make_tool();
        let error = tool
            .execute(
                json!({
                    "action": "run",
                    "goal": "inspect parser",
                    "mystery": true
                }),
                &make_ctx(),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn test_subagent_sync_run_returns_structured_result() {
        let tool = make_tool();
        let output = tool
            .execute(
                json!({
                    "action": "run",
                    "goal": "inspect parser"
                }),
                &make_ctx(),
            )
            .await
            .unwrap();
        let payload = parse_payload(&output);
        assert_eq!(payload["ok"], Value::Bool(true));
        assert!(payload["summary"].as_str().unwrap().contains("done"));
        assert!(payload["skill_name"].is_null());
    }

    #[tokio::test]
    #[serial]
    async fn test_subagent_sync_run_emits_trace_lifecycle_records() {
        let tool = make_tool();
        let run_id = unique_run_id("sync_subagent_trace_test");
        cleanup_trace_run(&run_id);

        let mut summary = RunSummary::new(
            &run_id,
            &run_id,
            "parent_trace_session",
            "parent_trace_session",
        );
        summary.started_at_unix_ms = 1;
        write_trace_summary(&summary);

        let mut ctx = ToolContext::new("parent_trace_session", "cli");
        ctx.trace = Some(crate::tools::protocol::ToolTraceContext {
            trace_id: run_id.clone(),
            run_id: run_id.clone(),
            root_session_id: "parent_trace_session".to_string(),
            task_id: None,
            turn_id: Some("turn_sync_trace".to_string()),
            iteration: Some(1),
            parent_span_id: Some("tool_span_sync_trace".to_string()),
        });

        let output = tool
            .execute(
                json!({
                    "action": "run",
                    "goal": "inspect parser"
                }),
                &ctx,
            )
            .await
            .unwrap();
        let payload = parse_payload(&output);
        let sub_session_id = payload["sub_session_id"]
            .as_str()
            .expect("sub_session_id")
            .to_string();

        let run = get_run(&run_id).expect("trace run should exist");
        assert_eq!(run.total_subagents, 1);

        let records = get_records(&run_id, &RecordQuery::default());
        assert!(records.iter().any(|record| {
            record.name == "subagent_spawned"
                && record.attrs.get("sub_session_id").and_then(Value::as_str)
                    == Some(sub_session_id.as_str())
        }));
        assert!(records.iter().any(|record| {
            record.name == "subagent_finished"
                && record.attrs.get("sub_session_id").and_then(Value::as_str)
                    == Some(sub_session_id.as_str())
        }));
        assert!(find_run_for_subsession("parent_trace_session", &sub_session_id).is_some());

        cleanup_trace_run(&run_id);
    }

    #[tokio::test]
    async fn test_subagent_background_spawn_and_status_can_consume_result() {
        let tool = make_tool();
        let spawned = tool
            .execute(
                json!({
                    "action": "run",
                    "goal": "inspect parser",
                    "background": true
                }),
                &make_ctx(),
            )
            .await
            .unwrap();
        let spawned_payload = parse_payload(&spawned);
        let job_id = spawned_payload["job_id"].as_str().unwrap().to_string();

        loop {
            let status = tool
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
            let payload = parse_payload(&status);
            if payload["status"] == Value::String("finished".to_string()) {
                assert!(payload["state"]["Completed"]["result"]["ok"].is_boolean());
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn test_subagent_sync_timeout_surfaces_error() {
        let tool = make_tool_with_llm(Arc::new(HangingLlm));
        let output = tool
            .execute(
                json!({
                    "action": "run",
                    "goal": "inspect parser",
                    "timeout_sec": 1
                }),
                &make_ctx(),
            )
            .await
            .unwrap();
        let payload = parse_payload(&output);
        assert_eq!(payload["ok"], Value::Bool(false));
        assert!(
            payload["summary"]
                .as_str()
                .unwrap()
                .contains("timed out after 1s")
        );
    }

    #[tokio::test]
    async fn test_subagent_rejects_goal_and_skill_name_together() {
        let tool = make_tool();
        let error = tool
            .execute(
                json!({
                    "action": "run",
                    "goal": "inspect parser",
                    "skill_name": "summarize_info"
                }),
                &make_ctx(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn test_subagent_skill_mode_rejects_interactive_skill() {
        let llm: Arc<dyn LlmClient> = Arc::new(FinishImmediatelyLlm);
        let base_tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(MockTool("read_file")),
            Arc::new(MockTool("subagent")),
        ];
        let runtime = SubagentRuntime::new(llm, base_tools, 2);
        let mut tool = SubagentTool::new(runtime);
        let mut registry = SkillRegistry::new();
        registry.insert(crate::skills::definition::SkillDef {
            meta: crate::skills::definition::SkillMeta {
                name: "interactive".to_string(),
                version: "1.0".to_string(),
                description: "interactive".to_string(),
                trigger: crate::skills::definition::SkillTrigger::ManualOnly,
                allowed_tools: vec!["ask_user_question".to_string()],
                output_mode: None,
            },
            instructions: "test".to_string(),
            parameters: None,
            constraints: crate::skills::definition::SkillConstraints::default(),
        });
        tool.registry = registry;

        let error = tool
            .execute(
                json!({
                    "action": "run",
                    "skill_name": "interactive"
                }),
                &make_ctx(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("interactive"));
    }

    #[tokio::test]
    async fn test_subagent_skill_mode_sync_returns_skill_metadata() {
        let tool = make_tool_with_base_tools(vec![Arc::new(MockTool("execute_bash"))]);
        let output = tool
            .execute(
                json!({
                    "action": "run",
                    "skill_name": "check_git_status",
                    "context": "Summarize the branch state.",
                    "max_steps": 6,
                    "timeout_sec": 30
                }),
                &make_ctx(),
            )
            .await
            .unwrap();

        let payload = parse_payload(&output);
        assert_eq!(payload["ok"], Value::Bool(true));
        assert_eq!(
            payload["skill_name"],
            Value::String("check_git_status".to_string())
        );
        assert_eq!(
            payload["effective_tools"],
            Value::Array(vec![Value::String("execute_bash".to_string())])
        );
        assert_eq!(payload["effective_max_steps"], Value::Number(6.into()));
        assert_eq!(payload["effective_timeout_sec"], Value::Number(30.into()));
    }

    #[tokio::test]
    async fn test_subagent_skill_mode_background_status_returns_skill_metadata() {
        let tool = make_tool_with_base_tools(vec![Arc::new(MockTool("execute_bash"))]);
        let spawned = tool
            .execute(
                json!({
                    "action": "run",
                    "skill_name": "check_git_status",
                    "background": true,
                    "context": "Summarize the branch state."
                }),
                &make_ctx(),
            )
            .await
            .unwrap();
        let spawned_payload = parse_payload(&spawned);
        let job_id = spawned_payload["job_id"].as_str().unwrap().to_string();
        assert_eq!(
            spawned_payload["skill_name"],
            Value::String("check_git_status".to_string())
        );

        loop {
            let status = tool
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
            let payload = parse_payload(&status);
            if payload["status"] == Value::String("finished".to_string()) {
                assert_eq!(
                    payload["state"]["Completed"]["result"]["skill_name"],
                    Value::String("check_git_status".to_string())
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn test_subagent_skill_mode_respects_nested_budget() {
        let tool = make_tool();
        let error = tool
            .execute(
                json!({
                    "action": "run",
                    "goal": "inspect parser"
                }),
                &make_skill_ctx(MAX_DELEGATION_CALLS_PER_ROOT_REQUEST, 8, 20),
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Nested delegation budget exceeded")
        );
    }

    #[tokio::test]
    async fn test_subagent_status_for_unknown_job_errors() {
        let tool = make_tool();
        let error = tool
            .execute(
                json!({
                    "action": "status",
                    "job_id": "missing"
                }),
                &make_ctx(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::ExecutionFailed(_)));
    }
}
