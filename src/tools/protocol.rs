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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{BashTool, PatchFileTool, ReadFileTool, ReadMemoryTool, WriteFileTool, WriteMemoryTool};

    #[test]
    fn test_tool_schema_validation() {
        let workspace = std::sync::Arc::new(crate::memory::WorkspaceMemory::new("test_memory.md"));
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(BashTool::new()),
            Box::new(ReadMemoryTool::new(workspace.clone())),
            Box::new(WriteMemoryTool::new(workspace)),
            Box::new(PatchFileTool),
            Box::new(WriteFileTool),
            Box::new(ReadFileTool),
        ];

        for tool in tools {
            let schema = tool.parameters_schema();
            let obj = schema.as_object().expect("Schema must be an object");

            assert!(!obj.contains_key("$schema"), "Schema for {} should not contain $schema", tool.name());
            assert!(!obj.contains_key("title"), "Schema for {} should not contain title", tool.name());

            if obj.get("type").and_then(|t| t.as_str()) == Some("object") {
                assert!(
                    obj.contains_key("properties"),
                    "Schema for {} must contain properties",
                    tool.name()
                );
            }
        }
    }

    #[test]
    fn test_clean_schema_removes_metadata_and_injects_properties_for_objects() {
        let schema = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Example",
            "type": "object"
        });

        let cleaned = clean_schema(schema);

        assert_eq!(cleaned.get("$schema"), None);
        assert_eq!(cleaned.get("title"), None);
        assert_eq!(cleaned["properties"], serde_json::json!({}));
    }

    #[test]
    fn test_serialize_tool_envelope_sets_expected_defaults() {
        let serialized = serialize_tool_envelope(
            "write_file",
            true,
            "ok".to_string(),
            Some(0),
            Some(42),
            false,
        )
        .unwrap();
        let envelope: ToolExecutionEnvelope = serde_json::from_str(&serialized).unwrap();

        assert!(envelope.ok);
        assert_eq!(envelope.tool_name, "write_file");
        assert_eq!(envelope.output, "ok");
        assert_eq!(envelope.exit_code, Some(0));
        assert_eq!(envelope.duration_ms, Some(42));
        assert!(!envelope.truncated);
        assert!(!envelope.recovery_attempted);
        assert_eq!(envelope.recovery_output, None);
        assert_eq!(envelope.recovery_rule, None);
    }
}
