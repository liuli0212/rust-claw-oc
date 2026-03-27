//! DispatchSubagent tool — creates a restricted, ephemeral sub-session.
//!
//! The sub-agent executes with:
//! - A subset of tools (configurable)
//! - A timeout (default 60 seconds)
//! - A maximum step count (controlled via energy)
//! - Summary-only context (no full parent history)

use async_trait::async_trait;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

use super::protocol::{clean_schema, StructuredToolOutput, Tool, ToolError};

pub struct DispatchSubagentTool {
    llm: Arc<dyn crate::llm_client::LlmClient>,
    base_tools: Vec<Arc<dyn Tool>>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DispatchSubagentArgs {
    /// The goal for the sub-agent to accomplish.
    pub goal: String,
    /// Summary of the context the sub-agent should work with.
    pub input_summary: String,
    /// Tools the sub-agent is allowed to use.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Timeout in seconds (default: 60).
    pub timeout_sec: Option<u64>,
    /// Maximum number of execution steps (default: 5).
    pub max_steps: Option<usize>,
}

/// Structured result returned by a sub-agent.
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

    /// Filter the base tool set to only include allowed tools.
    /// If `allowed_tools` is empty, use a safe default set (read_file, execute_bash, web_fetch).
    fn filter_tools(&self, allowed: &[String]) -> Vec<Arc<dyn Tool>> {
        let effective_allowed: Vec<String> = if allowed.is_empty() {
            // Safe default: read-only + bash
            vec![
                "read_file".to_string(),
                "execute_bash".to_string(),
                "web_fetch".to_string(),
            ]
        } else {
            allowed.to_vec()
        };

        // Always include finish_task and task_plan (runtime essentials)
        let runtime_tools = ["finish_task", "task_plan"];

        self.base_tools
            .iter()
            .filter(|tool| {
                let name = tool.name();
                runtime_tools.contains(&name.as_str())
                    || effective_allowed.contains(&name)
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
         The sub-agent runs with limited tools, timeout, and step count. \
         Returns a structured result with findings and artifacts."
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
        let max_steps = parsed.max_steps.unwrap_or(5);

        // Build constrained tool set
        let sub_tools = self.filter_tools(&parsed.allowed_tools);

        // Create a collector output that captures text
        let collector = Arc::new(CollectorOutput::new());

        // Construct a fresh, ephemeral AgentContext with the input summary as context
        let mut sub_context = crate::context::AgentContext::new();
        sub_context.system_prompts.push(format!(
            "You are a sub-agent with a STRICT mandate. \
             Complete the assigned goal and call `finish_task` with a summary.\n\
             Context from parent: {}\n\
             You have at most {} steps. Be concise and decisive.",
            parsed.input_summary, max_steps
        ));
        // Restrict max history tokens for sub-agent (much smaller)
        sub_context.max_history_tokens = 100_000;

        // Create ephemeral session ID
        let sub_session_id = format!(
            "sub_{}_{}", ctx.session_id,
            uuid::Uuid::new_v4().simple()
        );

        let (telemetry, _handle) = crate::telemetry::TelemetryExporter::new();
        let telemetry = Arc::new(telemetry);
        let task_state_store = Arc::new(crate::task_state::TaskStateStore::new(&sub_session_id));

        // Add finish_task and task_plan to sub_tools if not already present
        let mut final_tools = sub_tools;
        let has_finish = final_tools.iter().any(|t| t.name() == "finish_task");
        if !has_finish {
            final_tools.push(Arc::new(crate::tools::FinishTaskTool {
                task_state_store: task_state_store.clone(),
            }));
        }
        let has_plan = final_tools.iter().any(|t| t.name() == "task_plan");
        if !has_plan {
            final_tools.push(Arc::new(crate::tools::TaskPlanTool::new(
                sub_session_id.clone(),
                task_state_store.clone(),
            )));
        }

        let mut sub_loop = crate::core::AgentLoop::new(
            sub_session_id,
            self.llm.clone(),
            ctx.reply_to.clone(),
            final_tools,
            sub_context,
            collector.clone() as Arc<dyn crate::core::AgentOutput>,
            telemetry,
            task_state_store,
        );

        // Run the sub-agent with a timeout
        let run_result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_sec),
            sub_loop.step(parsed.goal.clone()),
        )
        .await;

        // Collect output
        let collected_text = collector.take_text().await;
        let tool_outputs = collector.take_tool_outputs().await;

        let result = match run_result {
            Ok(Ok(exit)) => {
                let summary = match &exit {
                    crate::core::RunExit::Finished(s) => s.clone(),
                    crate::core::RunExit::YieldedToUser => {
                        format!("Sub-agent yielded (text response). Output: {}", 
                            if collected_text.is_empty() { "(empty)" } else { &collected_text })
                    }
                    crate::core::RunExit::CriticallyFailed(e) => {
                        format!("Sub-agent failed: {}", e)
                    }
                    other => format!("Sub-agent exited: {:?}", other),
                };

                SubagentResult {
                    ok: matches!(exit, crate::core::RunExit::Finished(_) | crate::core::RunExit::YieldedToUser),
                    summary,
                    findings: tool_outputs,
                    artifacts: vec![],
                }
            }
            Ok(Err(e)) => SubagentResult {
                ok: false,
                summary: format!("Sub-agent error: {}", e),
                findings: vec![],
                artifacts: vec![],
            },
            Err(_) => SubagentResult {
                ok: false,
                summary: format!(
                    "Sub-agent timed out after {}s while working on: '{}'",
                    timeout_sec, parsed.goal
                ),
                findings: tool_outputs,
                artifacts: vec![],
            },
        };

        let output = serde_json::to_string_pretty(&result)
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        StructuredToolOutput::new(
            "dispatch_subagent",
            result.ok,
            output,
            None,
            None,
            false,
        )
        .to_json_string()
    }
}

// ---------------------------------------------------------------------------
// CollectorOutput — captures sub-agent output into buffers
// ---------------------------------------------------------------------------

struct CollectorOutput {
    text: AsyncMutex<String>,
    tool_outputs: AsyncMutex<Vec<String>>,
}

impl CollectorOutput {
    fn new() -> Self {
        Self {
            text: AsyncMutex::new(String::new()),
            tool_outputs: AsyncMutex::new(Vec::new()),
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
}

#[async_trait]
impl crate::core::AgentOutput for CollectorOutput {
    async fn on_text(&self, text: &str) {
        self.text.lock().await.push_str(text);
    }

    async fn on_thinking(&self, _text: &str) {
        // Silently discard thinking output
    }

    async fn on_tool_start(&self, _name: &str, _args: &str) {
        // Silent
    }

    async fn on_tool_end(&self, result: &str) {
        // Capture a truncated summary of each tool result
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
    use crate::tools::protocol::{ToolContext, ToolExecutionEnvelope};

    fn make_ctx() -> ToolContext {
        ToolContext {
            session_id: "test".to_string(),
            reply_to: "test".to_string(),
        }
    }

    #[test]
    fn test_filter_tools_default() {
        // Create a mock tool set
        struct MockTool(String);
        #[async_trait]
        impl Tool for MockTool {
            fn name(&self) -> String { self.0.clone() }
            fn description(&self) -> String { String::new() }
            fn parameters_schema(&self) -> Value { Value::Null }
            async fn execute(&self, _: Value, _: &ToolContext) -> Result<String, ToolError> {
                Ok(String::new())
            }
        }

        // We can't easily construct a real LlmClient in tests,
        // so we test the filter logic directly
        let base_tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(MockTool("read_file".to_string())),
            Arc::new(MockTool("write_file".to_string())),
            Arc::new(MockTool("execute_bash".to_string())),
            Arc::new(MockTool("finish_task".to_string())),
            Arc::new(MockTool("web_fetch".to_string())),
        ];

        // Create a dummy LLM
        struct DummyLlm;
        #[async_trait]
        impl crate::llm_client::LlmClient for DummyLlm {
            fn model_name(&self) -> &str { "dummy" }
            fn provider_name(&self) -> &str { "dummy" }
            async fn stream(
                &self,
                _: Vec<crate::context::Message>,
                _: Option<crate::context::Message>,
                _: Vec<Arc<dyn Tool>>,
            ) -> Result<tokio::sync::mpsc::Receiver<crate::llm_client::StreamEvent>, crate::llm_client::LlmError> {
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                drop(tx);
                Ok(rx)
            }
        }

        let tool = DispatchSubagentTool::new(Arc::new(DummyLlm), base_tools);

        // Empty allowed_tools → defaults (read_file, execute_bash, web_fetch + runtime)
        let filtered = tool.filter_tools(&[]);
        let names: Vec<String> = filtered.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"execute_bash".to_string()));
        assert!(names.contains(&"web_fetch".to_string()));
        assert!(names.contains(&"finish_task".to_string())); // runtime
        assert!(!names.contains(&"write_file".to_string())); // not in default
    }

    #[test]
    fn test_filter_tools_explicit() {
        struct MockTool(String);
        #[async_trait]
        impl Tool for MockTool {
            fn name(&self) -> String { self.0.clone() }
            fn description(&self) -> String { String::new() }
            fn parameters_schema(&self) -> Value { Value::Null }
            async fn execute(&self, _: Value, _: &ToolContext) -> Result<String, ToolError> {
                Ok(String::new())
            }
        }

        struct DummyLlm;
        #[async_trait]
        impl crate::llm_client::LlmClient for DummyLlm {
            fn model_name(&self) -> &str { "dummy" }
            fn provider_name(&self) -> &str { "dummy" }
            async fn stream(
                &self,
                _: Vec<crate::context::Message>,
                _: Option<crate::context::Message>,
                _: Vec<Arc<dyn Tool>>,
            ) -> Result<tokio::sync::mpsc::Receiver<crate::llm_client::StreamEvent>, crate::llm_client::LlmError> {
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                drop(tx);
                Ok(rx)
            }
        }

        let base_tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(MockTool("read_file".to_string())),
            Arc::new(MockTool("write_file".to_string())),
            Arc::new(MockTool("execute_bash".to_string())),
            Arc::new(MockTool("finish_task".to_string())),
        ];

        let tool = DispatchSubagentTool::new(Arc::new(DummyLlm), base_tools);

        // Explicit: only read_file
        let filtered = tool.filter_tools(&["read_file".to_string()]);
        let names: Vec<String> = filtered.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"finish_task".to_string())); // runtime always
        assert!(!names.contains(&"write_file".to_string()));
        assert!(!names.contains(&"execute_bash".to_string()));
    }

    #[tokio::test]
    async fn test_dispatch_subagent_invalid_args() {
        struct DummyLlm;
        #[async_trait]
        impl crate::llm_client::LlmClient for DummyLlm {
            fn model_name(&self) -> &str { "dummy" }
            fn provider_name(&self) -> &str { "dummy" }
            async fn stream(
                &self,
                _: Vec<crate::context::Message>,
                _: Option<crate::context::Message>,
                _: Vec<Arc<dyn Tool>>,
            ) -> Result<tokio::sync::mpsc::Receiver<crate::llm_client::StreamEvent>, crate::llm_client::LlmError> {
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                drop(tx);
                Ok(rx)
            }
        }

        let tool = DispatchSubagentTool::new(Arc::new(DummyLlm), vec![]);
        let args = serde_json::json!({"wrong_field": "value"});

        let result = tool.execute(args, &make_ctx()).await;
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    }
}
