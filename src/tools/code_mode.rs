use async_trait::async_trait;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};

use super::protocol::{clean_schema, serialize_tool_envelope, Tool, ToolContext, ToolError};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExecArgs {
    /// Raw JavaScript source used to orchestrate multiple nested tool calls.
    pub code: String,
    /// Optional host-driven progress publication interval in milliseconds.
    /// When set, the host may publish accumulated output while the cell keeps
    /// running in the background, even if the JS code does not call `flush()`.
    pub auto_flush_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct WaitArgs {
    pub cell_id: Option<String>,
    pub wait_timeout_ms: Option<u64>,
}

pub struct ExecTool;

pub struct WaitTool;

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> String {
        "exec".to_string()
    }

    fn description(&self) -> String {
        "Run JavaScript code to orchestrate multiple nested tool calls within a single model turn. Prefer this for multi-step coding work such as search-read-filter-patch-verify flows. If the JS schedules timers, polling, retries, or other long-running background work, usually set `auto_flush_ms` so progress can publish without a manual `flush()`."
            .to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schema_for!(ExecArgs)).unwrap())
    }

    fn has_side_effects(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<String, ToolError> {
        serialize_tool_envelope(
            "exec",
            false,
            "The `exec` tool must be dispatched through the code-mode service.".to_string(),
            Some(1),
            None,
            false,
        )
    }
}

#[async_trait]
impl Tool for WaitTool {
    fn name(&self) -> String {
        "wait".to_string()
    }

    fn description(&self) -> String {
        "Poll or sync with the currently pending code-mode cell for this session. Optionally provide a `cell_id` to assert which running cell should be observed. Set `wait_timeout_ms` to `0` for a non-blocking poll. `wait` does not resume timers; it only syncs the current cell state."
            .to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schema_for!(WaitArgs)).unwrap())
    }

    fn has_side_effects(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<String, ToolError> {
        serialize_tool_envelope(
            "wait",
            false,
            "The `wait` tool must be dispatched through the code-mode service.".to_string(),
            Some(1),
            None,
            false,
        )
    }
}
