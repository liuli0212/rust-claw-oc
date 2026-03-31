//! DispatchSubagent tool — creates a restricted, ephemeral sub-session.
//!
//! The sub-agent executes with:
//! - A subset of tools (configurable)
//! - A timeout (default 60 seconds)
//! - A maximum step count enforced via AgentLoop energy budget
//! - Summary-only context (no full parent history)

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::Instrument;

use super::protocol::{clean_schema, StructuredToolOutput, Tool, ToolError};

pub struct DispatchSubagentTool {
    llm: Arc<dyn crate::llm_client::LlmClient>,
    base_tools: Vec<Arc<dyn Tool>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DispatchSubagentArgs {
    pub goal: String,
    pub input_summary: String,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub claimed_paths: Vec<String>,
    #[serde(default)]
    pub allow_writes: bool,
    pub timeout_sec: Option<u64>,
    pub max_steps: Option<usize>,
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

impl DispatchSubagentTool {
    pub fn new(llm: Arc<dyn crate::llm_client::LlmClient>, base_tools: Vec<Arc<dyn Tool>>) -> Self {
        Self { llm, base_tools }
    }
}

#[async_trait]
impl Tool for DispatchSubagentTool {
    fn name(&self) -> String {
        "dispatch_subagent".to_string()
    }

    fn description(&self) -> String {
        "Dispatch a restricted sub-agent to perform an isolated task. \
         The sub-agent runs with limited tools, timeout, and enforced step count."
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
        ctx: &super::protocol::ToolContext,
    ) -> Result<String, ToolError> {
        tracing::debug!(
            "DispatchSubagentTool invoked within session: {}",
            ctx.session_id
        );
        let parsed: DispatchSubagentArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let timeout_sec = parsed.timeout_sec.unwrap_or(60);
        let max_steps = parsed.max_steps.unwrap_or(5).max(1);
        let goal = parsed.goal.clone();

        if parsed.allow_writes || !parsed.claimed_paths.is_empty() {
            return Err(ToolError::InvalidArguments(
                "dispatch_subagent does not support `allow_writes` or `claimed_paths`; use spawn_subagent for controlled background writes."
                    .to_string(),
            ));
        }

        tracing::info!(
            "Dispatching subagent with goal: '{}', timeout: {}s, max_steps: {}",
            goal,
            timeout_sec,
            max_steps
        );
        tracing::debug!("Subagent allowed_tools: {:?}", parsed.allowed_tools);

        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_notify = Arc::new(tokio::sync::Notify::new());
        let built = crate::session::factory::build_subagent_session(
            ctx,
            self.llm.clone(),
            &self.base_tools,
            crate::session::factory::SubagentBuildMode::SyncCompatible,
            None,
            &parsed.allowed_tools,
            max_steps,
            timeout_sec,
            &parsed.input_summary,
            crate::skills::call_tree::SkillSessionSeed::default(),
            std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::subagent_runtime::SubagentDebugSnapshot::default(),
            )),
            cancelled,
            cancel_notify,
        )
        .map_err(ToolError::ExecutionFailed)?;

        let crate::session::factory::BuiltSubagentSession {
            sub_session_id,
            transcript_path,
            event_log_path,
            mut agent_loop,
            collector,
            rejected_tools: _,
        } = built;
        let span = tracing::info_span!(
            "subagent_run_sync",
            parent_session_id = %ctx.session_id,
            sub_session_id = %sub_session_id,
            goal = %goal
        );

        let run_result = tokio::time::timeout(Duration::from_secs(timeout_sec), async move {
            agent_loop.step(goal).await
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
                            format!("Sub-agent yielded with output: {}", collected_text.trim())
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
                tracing::info!(target: "subagent", "[Sub:dispatch] {} with summary: {}", status_label, summary);

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
                        timeout_sec, parsed.goal
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
            "dispatch_subagent",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::protocol::ToolContext;

    fn make_ctx() -> ToolContext {
        ToolContext::new("test", "test")
    }

    struct MockTool(String);

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> String {
            self.0.clone()
        }

        fn description(&self) -> String {
            String::new()
        }

        fn parameters_schema(&self) -> Value {
            serde_json::json!({})
        }

        async fn execute(
            &self,
            _: Value,
            _: &crate::tools::protocol::ToolContext,
        ) -> Result<String, ToolError> {
            Ok(String::new())
        }
    }

    struct DummyLlm;

    #[async_trait]
    impl crate::llm_client::LlmClient for DummyLlm {
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
        ) -> Result<
            tokio::sync::mpsc::Receiver<crate::llm_client::StreamEvent>,
            crate::llm_client::LlmError,
        > {
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            Ok(rx)
        }
    }

    #[test]
    fn test_filter_tools_default() {
        let base_tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(MockTool("read_file".to_string())),
            Arc::new(MockTool("write_file".to_string())),
            Arc::new(MockTool("execute_bash".to_string())),
            Arc::new(MockTool("finish_task".to_string())),
            Arc::new(MockTool("web_fetch".to_string())),
        ];
        let tool = DispatchSubagentTool::new(Arc::new(DummyLlm), base_tools);

        let (filtered, _) = crate::session::factory::filter_subagent_tools(
            &tool.base_tools,
            &[],
            crate::session::factory::SubagentBuildMode::SyncCompatible,
        );
        let names: Vec<String> = filtered.iter().map(|tool| tool.name()).collect();
        assert!(names.contains(&"read_file".to_string()));
        assert!(!names.contains(&"execute_bash".to_string()));
        assert!(names.contains(&"web_fetch".to_string()));
        assert!(names.contains(&"finish_task".to_string()));
        assert!(!names.contains(&"write_file".to_string()));
    }

    #[test]
    fn test_filter_tools_explicit() {
        let base_tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(MockTool("read_file".to_string())),
            Arc::new(MockTool("write_file".to_string())),
            Arc::new(MockTool("execute_bash".to_string())),
            Arc::new(MockTool("finish_task".to_string())),
            Arc::new(MockTool("dispatch_subagent".to_string())),
        ];
        let tool = DispatchSubagentTool::new(Arc::new(DummyLlm), base_tools);

        let (filtered, _) = crate::session::factory::filter_subagent_tools(
            &tool.base_tools,
            &["read_file".to_string(), "dispatch_subagent".to_string()],
            crate::session::factory::SubagentBuildMode::SyncCompatible,
        );
        let names: Vec<String> = filtered.iter().map(|tool| tool.name()).collect();
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"finish_task".to_string()));
        assert!(!names.contains(&"write_file".to_string()));
        assert!(!names.contains(&"execute_bash".to_string()));
        assert!(!names.contains(&"dispatch_subagent".to_string())); // Should be filtered out
    }

    #[tokio::test]
    async fn test_dispatch_subagent_invalid_args() {
        let tool = DispatchSubagentTool::new(Arc::new(DummyLlm), vec![]);
        let args = serde_json::json!({ "wrong_field": "value" });

        let result = tool.execute(args, &make_ctx()).await;
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    }

    #[tokio::test]
    async fn test_dispatch_subagent_rejects_sync_write_controls() {
        let tool = DispatchSubagentTool::new(Arc::new(DummyLlm), vec![]);
        let args = serde_json::json!({
            "goal": "Edit parser",
            "input_summary": "repo context",
            "allow_writes": true,
            "claimed_paths": ["src/parser.rs"]
        });

        let result = tool.execute(args, &make_ctx()).await;
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    }

    #[tokio::test]
    async fn test_dispatch_subagent_reports_energy_exhaustion_when_max_steps_is_one() {
        let tool = DispatchSubagentTool::new(Arc::new(DummyLlm), vec![]);
        let args = serde_json::json!({
            "goal": "Investigate repository",
            "input_summary": "repo context",
            "max_steps": 1,
            "timeout_sec": 5
        });

        let result = tool.execute(args, &make_ctx()).await.unwrap();
        let envelope: crate::tools::protocol::ToolExecutionEnvelope =
            serde_json::from_str(&result).unwrap();
        assert!(!envelope.result.ok);
        assert!(envelope.result.output.contains("Structural summary"));
        assert!(envelope.result.output.contains("sub_session_id"));
        assert!(envelope.result.output.contains("transcript_path"));
        assert!(envelope.result.output.contains("event_log_path"));
    }

    #[test]
    fn test_filter_tools_async_readonly_blocks_write_like_tools() {
        let base_tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(MockTool("read_file".to_string())),
            Arc::new(MockTool("write_file".to_string())),
            Arc::new(MockTool("patch_file".to_string())),
            Arc::new(MockTool("execute_bash".to_string())),
            Arc::new(MockTool("finish_task".to_string())),
        ];
        let (filtered, _) = crate::session::factory::filter_subagent_tools(
            &base_tools,
            &[
                "read_file".to_string(),
                "write_file".to_string(),
                "patch_file".to_string(),
                "execute_bash".to_string(),
            ],
            crate::session::factory::SubagentBuildMode::AsyncReadonly,
        );
        let names: Vec<String> = filtered.iter().map(|tool| tool.name()).collect();
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"finish_task".to_string()));
        assert!(!names.contains(&"write_file".to_string()));
        assert!(!names.contains(&"patch_file".to_string()));
        assert!(!names.contains(&"execute_bash".to_string()));
    }
}
