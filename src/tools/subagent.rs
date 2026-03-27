//! DispatchSubagent tool — creates a restricted, ephemeral sub-session.
//!
//! The sub-agent executes with:
//! - A subset of tools (configurable)
//! - A timeout (default 60 seconds)
//! - A maximum step count (default 5)
//! - Summary-only context (no full parent history)

use async_trait::async_trait;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::protocol::{clean_schema, StructuredToolOutput, Tool, ToolError};

pub struct DispatchSubagentTool;

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
    pub fn new() -> Self {
        Self
    }
}

impl Default for DispatchSubagentTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for DispatchSubagentTool {
    fn name(&self) -> String {
        "dispatch_subagent".to_string()
    }

    fn description(&self) -> String {
        "Dispatch a restricted sub-agent to perform an isolated task. \
         The sub-agent runs with limited tools, timeout, and step count."
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
        _ctx: &super::protocol::ToolContext,
    ) -> Result<String, ToolError> {
        let parsed: DispatchSubagentArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let _timeout_sec = parsed.timeout_sec.unwrap_or(60);
        let _max_steps = parsed.max_steps.unwrap_or(5);

        // Phase 4 implementation stub:
        // In production, this would call SessionManager::create_ephemeral_session()
        // to spawn a restricted AgentLoop with the given constraints.
        //
        // For now, return a structured placeholder indicating the sub-agent
        // received the request but the execution engine is not yet wired.
        let result = SubagentResult {
            ok: true,
            summary: format!(
                "Sub-agent dispatched for goal: '{}'. \
                 Context: '{}'. Allowed tools: {:?}. \
                 Timeout: {}s, Max steps: {}.",
                parsed.goal,
                parsed.input_summary,
                parsed.allowed_tools,
                _timeout_sec,
                _max_steps
            ),
            findings: vec![
                "Sub-agent execution engine not yet wired (Phase 4 stub)".to_string(),
            ],
            artifacts: vec![],
        };

        let output = serde_json::to_string_pretty(&result)
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        StructuredToolOutput::new(
            "dispatch_subagent",
            true,
            output,
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
    use crate::tools::protocol::{ToolContext, ToolExecutionEnvelope};

    fn make_ctx() -> ToolContext {
        ToolContext {
            session_id: "test".to_string(),
            reply_to: "test".to_string(),
        }
    }

    #[tokio::test]
    async fn test_dispatch_subagent_basic() {
        let tool = DispatchSubagentTool::new();
        let args = serde_json::json!({
            "goal": "Review database schema",
            "input_summary": "Current schema has 5 tables"
        });

        let result = tool.execute(args, &make_ctx()).await.unwrap();
        let envelope: ToolExecutionEnvelope = serde_json::from_str(&result).unwrap();

        assert!(envelope.ok);
        assert!(envelope.output.contains("Review database schema"));
    }

    #[tokio::test]
    async fn test_dispatch_subagent_with_constraints() {
        let tool = DispatchSubagentTool::new();
        let args = serde_json::json!({
            "goal": "Analyze code",
            "input_summary": "Check main.rs",
            "allowed_tools": ["read_file"],
            "timeout_sec": 30,
            "max_steps": 3
        });

        let result = tool.execute(args, &make_ctx()).await.unwrap();
        let envelope: ToolExecutionEnvelope = serde_json::from_str(&result).unwrap();

        assert!(envelope.ok);
        assert!(envelope.output.contains("read_file"));
        assert!(envelope.output.contains("30s"));
    }

    #[tokio::test]
    async fn test_dispatch_subagent_invalid_args() {
        let tool = DispatchSubagentTool::new();
        let args = serde_json::json!({
            "wrong_field": "value"
        });

        let result = tool.execute(args, &make_ctx()).await;
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    }
}
