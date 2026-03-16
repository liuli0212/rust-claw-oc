use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub fn clean_schema(mut schema_val: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = schema_val.as_object_mut() {
        obj.remove("$schema");
        obj.remove("title");
        if obj.get("type").and_then(|t| t.as_str()) == Some("object")
            && !obj.contains_key("properties")
        {
            obj.insert("properties".to_string(), serde_json::json!({}));
        }
    }
    schema_val
}

#[derive(Error, Debug)]
pub enum ToolError {
    #[error("Execution failed: {0}")]
    ExecutionFailed(String),
    #[error("Invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("Timeout")]
    Timeout,
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> String;
    fn description(&self) -> String;
    fn parameters_schema(&self) -> serde_json::Value;
    async fn execute(&self, args: Value) -> Result<String, ToolError>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StructuredToolOutput {
    pub ok: bool,
    pub tool_name: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u128>,
    pub truncated: bool,
    pub recovery_attempted: bool,
    pub recovery_output: Option<String>,
    pub recovery_rule: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ToolExecutionEnvelope {
    pub ok: bool,
    pub tool_name: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u128>,
    pub truncated: bool,
    #[serde(default)]
    pub recovery_attempted: bool,
    #[serde(default)]
    pub recovery_output: Option<String>,
    #[serde(default)]
    pub recovery_rule: Option<String>,
}

impl StructuredToolOutput {
    pub fn new(
        tool_name: impl Into<String>,
        ok: bool,
        output: String,
        exit_code: Option<i32>,
        duration_ms: Option<u128>,
        truncated: bool,
    ) -> Self {
        Self {
            ok,
            tool_name: tool_name.into(),
            output,
            exit_code,
            duration_ms,
            truncated,
            recovery_attempted: false,
            recovery_output: None,
            recovery_rule: None,
        }
    }

    pub fn into_envelope(self) -> ToolExecutionEnvelope {
        ToolExecutionEnvelope {
            ok: self.ok,
            tool_name: self.tool_name,
            output: self.output,
            exit_code: self.exit_code,
            duration_ms: self.duration_ms,
            truncated: self.truncated,
            recovery_attempted: self.recovery_attempted,
            recovery_output: self.recovery_output,
            recovery_rule: self.recovery_rule,
        }
    }

    pub fn to_json_string(self) -> Result<String, ToolError> {
        serde_json::to_string(&self.into_envelope())
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))
    }
}

pub fn serialize_tool_envelope(
    tool_name: &str,
    ok: bool,
    output: String,
    exit_code: Option<i32>,
    duration_ms: Option<u128>,
    truncated: bool,
) -> Result<String, ToolError> {
    StructuredToolOutput::new(tool_name, ok, output, exit_code, duration_ms, truncated)
        .to_json_string()
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct EmptyArgs {}
