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
use tokio::sync::Mutex as AsyncMutex;

use super::protocol::{clean_schema, StructuredToolOutput, Tool, ToolError};

pub struct DispatchSubagentTool {
    llm: Arc<dyn crate::llm_client::LlmClient>,
    base_tools: Vec<Arc<dyn Tool>>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DispatchSubagentArgs {
    pub goal: String,
    pub input_summary: String,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    pub timeout_sec: Option<u64>,
    pub max_steps: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentResult {
    pub ok: bool,
    pub summary: String,
    pub findings: Vec<String>,
    pub artifacts: Vec<String>,
}

impl DispatchSubagentTool {
    pub fn new(
        llm: Arc<dyn crate::llm_client::LlmClient>,
        base_tools: Vec<Arc<dyn Tool>>,
    ) -> Self {
        Self { llm, base_tools }
    }

    fn filter_tools(&self, allowed: &[String]) -> Vec<Arc<dyn Tool>> {
        let effective_allowed: Vec<String> = if allowed.is_empty() {
            vec![
                "read_file".to_string(),
                "web_fetch".to_string(),
            ]
        } else {
            allowed.to_vec()
        };

        let runtime_tools = ["finish_task", "task_plan"];

        self.base_tools
            .iter()
            .filter(|tool| {
                let name = tool.name();
                name != "dispatch_subagent" && (runtime_tools.contains(&name.as_str()) || effective_allowed.contains(&name))
            })
            .cloned()
            .collect()
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
        let parsed: DispatchSubagentArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let timeout_sec = parsed.timeout_sec.unwrap_or(60);
        let max_steps = parsed.max_steps.unwrap_or(5).max(1);

        let collector = Arc::new(CollectorOutput::new());
        let mut context = crate::context::AgentContext::new();
        
        let mut prompt = format!(
            "You are a restricted sub-agent. Complete the assigned goal with the available tools, \
             then call `finish_task`.\nParent context summary:\n{}\n\nBe concise.",
            parsed.input_summary
        );

        if let Ok(memory) = std::fs::read_to_string("MEMORY.md") {
            prompt.push_str(&format!("\n\nWorkspace Memory:\n{}", memory));
        }
        if let Ok(agents_md) = std::fs::read_to_string("AGENTS.md") {
            prompt.push_str(&format!("\n\nAgent Guidelines:\n{}", agents_md));
        }

        context.system_prompts.push(prompt);
        context.max_history_tokens = 100_000;

        let sub_session_id = format!("sub_{}_{}", ctx.session_id, uuid::Uuid::new_v4().simple());
        let (telemetry, _handle) = crate::telemetry::TelemetryExporter::new();
        let telemetry = Arc::new(telemetry);
        let task_state_store = Arc::new(crate::task_state::TaskStateStore::new(&sub_session_id));

        let mut tools = self.filter_tools(&parsed.allowed_tools);
        if !tools.iter().any(|tool| tool.name() == "task_plan") {
            tools.push(Arc::new(crate::tools::TaskPlanTool::new(
                sub_session_id.clone(),
                task_state_store.clone(),
            )));
        }
        if !tools.iter().any(|tool| tool.name() == "finish_task") {
            tools.push(Arc::new(crate::tools::FinishTaskTool {
                task_state_store: task_state_store.clone(),
            }));
        }

        let mut sub_loop = crate::core::AgentLoop::new(
            sub_session_id,
            self.llm.clone(),
            ctx.reply_to.clone(),
            tools,
            context,
            collector.clone() as Arc<dyn crate::core::AgentOutput>,
            telemetry,
            task_state_store,
        );
        sub_loop.set_initial_energy_budget(max_steps);

        let run_result = tokio::time::timeout(
            Duration::from_secs(timeout_sec),
            sub_loop.step(parsed.goal.clone()),
        )
        .await;

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
                    crate::core::RunExit::StoppedByUser => {
                        "Sub-agent execution was interrupted.".to_string()
                    }
                };

                SubagentResult {
                    ok,
                    summary,
                    findings: tool_outputs,
                    artifacts,
                }
            }
            Ok(Err(error)) => SubagentResult {
                ok: false,
                summary: format!("Sub-agent error: {}", error),
                findings: tool_outputs,
                artifacts,
            },
            Err(_) => SubagentResult {
                ok: false,
                summary: format!(
                    "Sub-agent timed out after {}s while working on '{}'.",
                    timeout_sec, parsed.goal
                ),
                findings: tool_outputs,
                artifacts,
            },
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

struct CollectorOutput {
    text: AsyncMutex<String>,
    tool_outputs: AsyncMutex<Vec<String>>,
    artifacts: AsyncMutex<Vec<String>>,
}

impl CollectorOutput {
    fn new() -> Self {
        Self {
            text: AsyncMutex::new(String::new()),
            tool_outputs: AsyncMutex::new(Vec::new()),
            artifacts: AsyncMutex::new(Vec::new()),
        }
    }

    async fn take_text(&self) -> String {
        let mut text = self.text.lock().await;
        std::mem::take(&mut *text)
    }

    async fn take_tool_outputs(&self) -> Vec<String> {
        let mut outputs = self.tool_outputs.lock().await;
        std::mem::take(&mut *outputs)
    }

    async fn take_artifacts(&self) -> Vec<String> {
        let mut artifacts = self.artifacts.lock().await;
        std::mem::take(&mut *artifacts)
    }
}

#[async_trait]
impl crate::core::AgentOutput for CollectorOutput {
    async fn on_text(&self, text: &str) {
        self.text.lock().await.push_str(text);
    }

    async fn on_thinking(&self, _text: &str) {}

    async fn on_tool_start(&self, name: &str, args: &str) {
        if name == "write_file" || name == "patch_file" {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(args) {
                if let Some(path) = parsed.get("path").and_then(|p| p.as_str()) {
                    self.artifacts.lock().await.push(path.to_string());
                }
            }
        }
    }

    async fn on_tool_end(&self, result: &str) {
        let truncated = if result.len() > 500 {
            format!("{}...(truncated)", &result[..500])
        } else {
            result.to_string()
        };
        self.tool_outputs.lock().await.push(truncated);
    }

    async fn on_error(&self, error: &str) {
        self.text.lock().await.push_str(&format!("[ERROR] {}\n", error));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::protocol::ToolContext;

    fn make_ctx() -> ToolContext {
        ToolContext {
            session_id: "test".to_string(),
            reply_to: "test".to_string(),
        }
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

        let filtered = tool.filter_tools(&[]);
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

        let filtered = tool.filter_tools(&["read_file".to_string(), "dispatch_subagent".to_string()]);
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
        assert!(envelope.result.output.contains("Energy depleted"));
    }
}
