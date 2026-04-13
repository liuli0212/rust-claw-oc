use async_trait::async_trait;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};

use super::protocol::{clean_schema, serialize_tool_envelope, Tool, ToolContext, ToolError};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExecArgs {
    /// Raw JavaScript source used to orchestrate multiple nested tool calls.
    pub code: String,
}

pub struct ExecTool;

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> String {
        "exec".to_string()
    }

    fn description(&self) -> String {
        "Run JavaScript code to orchestrate multiple nested tool calls within a single model turn. Prefer this for multi-step coding work such as search-read-filter-patch-verify flows."
            .to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schema_for!(ExecArgs)).unwrap())
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
