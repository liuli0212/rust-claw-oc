use async_trait::async_trait;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::subagent_runtime::SubagentRuntime;
use crate::tools::protocol::{clean_schema, StructuredToolOutput, Tool, ToolError};
use crate::tools::subagent::DispatchSubagentArgs;

pub struct SpawnSubagentTool {
    runtime: SubagentRuntime,
}

impl SpawnSubagentTool {
    pub fn new(runtime: SubagentRuntime) -> Self {
        Self { runtime }
    }
}

pub struct GetSubagentResultTool {
    runtime: SubagentRuntime,
}

impl GetSubagentResultTool {
    pub fn new(runtime: SubagentRuntime) -> Self {
        Self { runtime }
    }
}

pub struct CancelSubagentTool {
    runtime: SubagentRuntime,
}

impl CancelSubagentTool {
    pub fn new(runtime: SubagentRuntime) -> Self {
        Self { runtime }
    }
}

pub struct ListSubagentJobsTool {
    runtime: SubagentRuntime,
}

impl ListSubagentJobsTool {
    pub fn new(runtime: SubagentRuntime) -> Self {
        Self { runtime }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct JobIdArgs {
    job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct GetSubagentResultArgs {
    job_id: String,
    #[serde(default)]
    consume: bool,
    /// Optional: block up to this many seconds waiting for the subagent to finish.
    /// If omitted, returns immediately with current status.
    #[serde(default)]
    wait_sec: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct EmptyArgs {}

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

#[async_trait]
impl Tool for SpawnSubagentTool {
    fn name(&self) -> String {
        "spawn_subagent".to_string()
    }

    fn description(&self) -> String {
        "Start a background subagent and return a job ID immediately. Use this for independent work that can continue while you do other tasks. Background subagents run read-only by default; if you set allow_writes=true, you must also provide non-overlapping claimed_paths and only file mutation tools are enabled."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        clean_schema(serde_json::to_value(schema_for!(DispatchSubagentArgs)).unwrap())
    }

    fn has_side_effects(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &crate::tools::ToolContext,
    ) -> Result<String, ToolError> {
        let parsed: DispatchSubagentArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let spawned = self.runtime.spawn_job(ctx.clone(), parsed).await?;
        serialize_output(
            "spawn_subagent",
            json!({
                "job_id": spawned.job_id,
                "sub_session_id": spawned.sub_session_id,
                "status": "spawned",
            }),
        )
    }
}

#[async_trait]
impl Tool for GetSubagentResultTool {
    fn name(&self) -> String {
        "get_subagent_result".to_string()
    }

    fn description(&self) -> String {
        "Get the current status or final result of a background subagent job. \
         Set `wait_sec` (e.g. 10) to block and wait for completion instead of \
         polling repeatedly. Do NOT call this tool in a tight loop without wait_sec."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        clean_schema(serde_json::to_value(schema_for!(GetSubagentResultArgs)).unwrap())
    }

    fn has_side_effects(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &crate::tools::ToolContext,
    ) -> Result<String, ToolError> {
        let parsed: GetSubagentResultArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        // Long polling: if wait_sec is set and job is not yet terminal, block.
        if let Some(wait_sec) = parsed.wait_sec {
            if let Some(handle) = self.runtime.get_job_handle(&parsed.job_id).await {
                let deadline =
                    tokio::time::Instant::now() + std::time::Duration::from_secs(wait_sec);
                loop {
                    let state = handle.state.read().await;
                    if state.is_terminal() {
                        break;
                    }
                    drop(state);

                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        break;
                    }

                    // Wait for completion signal OR heartbeat (2s max to guard against
                    // lost Notify signals — see implementation_plan.md #2 rationale).
                    tokio::select! {
                        _ = handle.completion_notify.notified() => {}
                        _ = tokio::time::sleep(remaining.min(std::time::Duration::from_secs(2))) => {}
                    }
                }
            }
        }

        let snapshot = self
            .runtime
            .get_job_snapshot(&parsed.job_id, parsed.consume)
            .await?;

        if snapshot.state.is_terminal() {
            let status = snapshot.state.finish_reason();
            let summary_text = match &snapshot.state {
                crate::subagent_runtime::SubagentJobState::Completed { result, .. } => {
                    result.summary.clone()
                }
                crate::subagent_runtime::SubagentJobState::Failed { error, partial, .. } => partial
                    .as_ref()
                    .map(|p| p.summary.clone())
                    .unwrap_or_else(|| error.clone()),
                crate::subagent_runtime::SubagentJobState::Cancelled { partial, .. } => partial
                    .as_ref()
                    .map(|p| p.summary.clone())
                    .unwrap_or_else(|| "Cancelled".to_string()),
                crate::subagent_runtime::SubagentJobState::TimedOut { partial, .. } => partial
                    .as_ref()
                    .map(|p| p.summary.clone())
                    .unwrap_or_else(|| "Timed out".to_string()),
                _ => String::new(),
            };
            tracing::info!(target: "subagent", "[Sub:{}] Fetched {} result: {}", parsed.job_id, status, summary_text);
        }

        serialize_output(
            "get_subagent_result",
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
}

#[async_trait]
impl Tool for CancelSubagentTool {
    fn name(&self) -> String {
        "cancel_subagent".to_string()
    }

    fn description(&self) -> String {
        "Cancel a running background subagent job by job ID.".to_string()
    }

    fn parameters_schema(&self) -> Value {
        clean_schema(serde_json::to_value(schema_for!(JobIdArgs)).unwrap())
    }

    fn has_side_effects(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &crate::tools::ToolContext,
    ) -> Result<String, ToolError> {
        let parsed: JobIdArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        self.runtime.cancel_job(&parsed.job_id).await?;
        serialize_output(
            "cancel_subagent",
            json!({
                "job_id": parsed.job_id,
                "status": "cancelling",
            }),
        )
    }
}

#[async_trait]
impl Tool for ListSubagentJobsTool {
    fn name(&self) -> String {
        "list_subagent_jobs".to_string()
    }

    fn description(&self) -> String {
        "List known background subagent jobs and their current states.".to_string()
    }

    fn parameters_schema(&self) -> Value {
        clean_schema(serde_json::to_value(schema_for!(EmptyArgs)).unwrap())
    }

    fn has_side_effects(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        _args: Value,
        _ctx: &crate::tools::ToolContext,
    ) -> Result<String, ToolError> {
        let jobs = self.runtime.list_jobs().await;
        serialize_output(
            "list_subagent_jobs",
            json!({
                "jobs": jobs,
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use tokio::sync::mpsc;

    use crate::context::{FunctionCall, Message};
    use crate::llm_client::{LlmClient, LlmError, StreamEvent};

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

        async fn execute(
            &self,
            _args: Value,
            _ctx: &crate::tools::ToolContext,
        ) -> Result<String, ToolError> {
            Ok(String::new())
        }
    }

    fn make_ctx() -> crate::tools::ToolContext {
        crate::tools::ToolContext::new("parent", "cli")
    }

    #[tokio::test]
    async fn test_spawn_and_get_subagent_result() {
        let runtime = SubagentRuntime::new(
            Arc::new(FinishImmediatelyLlm),
            vec![Arc::new(MockTool("read_file"))],
            2,
        );
        let spawn_tool = SpawnSubagentTool::new(runtime.clone());
        let get_tool = GetSubagentResultTool::new(runtime);

        let spawned = spawn_tool
            .execute(
                json!({
                    "goal": "inspect",
                    "input_summary": "summary",
                    "allowed_tools": ["read_file"],
                    "timeout_sec": 20,
                    "max_steps": 4
                }),
                &make_ctx(),
            )
            .await
            .unwrap();
        let spawned_env = crate::tools::protocol::ToolExecutionEnvelope::from_json_str(&spawned)
            .expect("spawn envelope");
        let spawned_json: Value = serde_json::from_str(&spawned_env.result.output).unwrap();
        let job_id = spawned_json["job_id"].as_str().unwrap().to_string();

        let mut status = String::new();
        for _ in 0..400 {
            let result = get_tool
                .execute(json!({ "job_id": job_id }), &make_ctx())
                .await
                .unwrap();
            let env = crate::tools::protocol::ToolExecutionEnvelope::from_json_str(&result)
                .expect("get envelope");
            let payload: Value = serde_json::from_str(&env.result.output).unwrap();
            status = payload["status"].as_str().unwrap_or_default().to_string();
            if status == "finished" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        assert_eq!(status, "finished");
    }

    #[tokio::test]
    async fn test_cancel_subagent_unknown_job_returns_error() {
        let runtime = SubagentRuntime::new(
            Arc::new(FinishImmediatelyLlm),
            vec![Arc::new(MockTool("read_file"))],
            2,
        );
        let cancel_tool = CancelSubagentTool::new(runtime);
        let err = cancel_tool
            .execute(json!({ "job_id": "missing" }), &make_ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(_)));
    }

    #[tokio::test]
    async fn test_get_subagent_result_can_consume_terminal_job() {
        let runtime = SubagentRuntime::new(
            Arc::new(FinishImmediatelyLlm),
            vec![Arc::new(MockTool("read_file"))],
            2,
        );
        let spawn_tool = SpawnSubagentTool::new(runtime.clone());
        let get_tool = GetSubagentResultTool::new(runtime);

        let spawned = spawn_tool
            .execute(
                json!({
                    "goal": "inspect",
                    "input_summary": "summary",
                    "allowed_tools": ["read_file"],
                    "timeout_sec": 20,
                    "max_steps": 4
                }),
                &make_ctx(),
            )
            .await
            .unwrap();
        let spawned_env = crate::tools::protocol::ToolExecutionEnvelope::from_json_str(&spawned)
            .expect("spawn envelope");
        let spawned_json: Value = serde_json::from_str(&spawned_env.result.output).unwrap();
        let job_id = spawned_json["job_id"].as_str().unwrap().to_string();

        let mut consumed_at = None;
        for _ in 0..400 {
            let result = get_tool
                .execute(json!({ "job_id": job_id, "consume": true }), &make_ctx())
                .await
                .unwrap();
            let env = crate::tools::protocol::ToolExecutionEnvelope::from_json_str(&result)
                .expect("get envelope");
            let payload: Value = serde_json::from_str(&env.result.output).unwrap();
            if payload["status"].as_str().unwrap_or_default() == "finished" {
                assert_eq!(payload["consumed"], Value::Bool(true));
                consumed_at = payload["consumed_at_unix_ms"].as_u64();
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        assert!(consumed_at.is_some());

        let result = get_tool
            .execute(json!({ "job_id": job_id }), &make_ctx())
            .await
            .unwrap();
        let env = crate::tools::protocol::ToolExecutionEnvelope::from_json_str(&result)
            .expect("get envelope");
        let payload: Value = serde_json::from_str(&env.result.output).unwrap();
        assert_eq!(payload["consumed"], Value::Bool(true));
        assert_eq!(payload["consumed_at_unix_ms"].as_u64(), consumed_at);
    }

    #[tokio::test]
    async fn test_get_subagent_result_exposes_debug_snapshot() {
        let runtime = SubagentRuntime::new(
            Arc::new(FinishImmediatelyLlm),
            vec![Arc::new(MockTool("read_file"))],
            2,
        );
        let spawn_tool = SpawnSubagentTool::new(runtime.clone());
        let get_tool = GetSubagentResultTool::new(runtime);

        let spawned = spawn_tool
            .execute(
                json!({
                    "goal": "inspect",
                    "input_summary": "summary",
                    "allowed_tools": ["read_file"],
                    "timeout_sec": 20,
                    "max_steps": 4
                }),
                &make_ctx(),
            )
            .await
            .unwrap();
        let spawned_env = crate::tools::protocol::ToolExecutionEnvelope::from_json_str(&spawned)
            .expect("spawn envelope");
        let spawned_json: Value = serde_json::from_str(&spawned_env.result.output).unwrap();
        let job_id = spawned_json["job_id"].as_str().unwrap().to_string();

        let mut payload = Value::Null;
        for _ in 0..400 {
            let result = get_tool
                .execute(json!({ "job_id": job_id }), &make_ctx())
                .await
                .unwrap();
            let env = crate::tools::protocol::ToolExecutionEnvelope::from_json_str(&result)
                .expect("get envelope");
            payload = serde_json::from_str(&env.result.output).unwrap();
            if payload["status"].as_str().unwrap_or_default() == "finished" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        assert_eq!(
            payload["debug"]["state_label"],
            Value::String("finished".to_string())
        );
        assert!(payload["debug"]["updated_at_unix_ms"].as_u64().is_some());
    }
}
