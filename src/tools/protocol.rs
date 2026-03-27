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

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub session_id: String,
    pub reply_to: String,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> String;
    fn description(&self) -> String;
    fn parameters_schema(&self) -> serde_json::Value;
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError>;
    /// Whether this tool can modify files, state, or the outside world.
    /// Read-only tools should return false. Default is true (conservative).
    fn has_side_effects(&self) -> bool {
        true
    }
}

/// Structured question to present to the user during skill execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserPromptRequest {
    pub question: String,
    pub context_key: String,
    pub options: Vec<String>,
    pub recommendation: Option<String>,
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
    pub file_path: Option<String>,
    pub evidence_kind: Option<String>,
    pub evidence_source_path: Option<String>,
    pub evidence_summary: Option<String>,
    pub payload_kind: Option<String>,
    pub invalidate_diagnostic_evidence: bool,
    pub finish_task_summary: Option<String>,
    /// If set, the tool is requesting that execution pause for user input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub await_user: Option<UserPromptRequest>,
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
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub evidence_kind: Option<String>,
    #[serde(default)]
    pub evidence_source_path: Option<String>,
    #[serde(default)]
    pub evidence_summary: Option<String>,
    #[serde(default)]
    pub payload_kind: Option<String>,
    #[serde(default)]
    pub invalidate_diagnostic_evidence: bool,
    #[serde(default)]
    pub finish_task_summary: Option<String>,
    #[serde(default)]
    pub await_user: Option<UserPromptRequest>,
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
            file_path: None,
            evidence_kind: None,
            evidence_source_path: None,
            evidence_summary: None,
            payload_kind: None,
            invalidate_diagnostic_evidence: false,
            finish_task_summary: None,
            await_user: None,
        }
    }

    pub fn with_file_path(mut self, path: impl Into<String>) -> Self {
        self.file_path = Some(path.into());
        self
    }

    pub fn with_evidence(
        mut self,
        kind: impl Into<String>,
        source_path: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        self.evidence_kind = Some(kind.into());
        self.evidence_source_path = Some(source_path.into());
        self.evidence_summary = Some(summary.into());
        self
    }

    pub fn with_invalidated_diagnostics(mut self) -> Self {
        self.invalidate_diagnostic_evidence = true;
        self
    }

    pub fn with_payload_kind(mut self, kind: impl Into<String>) -> Self {
        self.payload_kind = Some(kind.into());
        self
    }

    pub fn with_finish_task_summary(mut self, summary: impl Into<String>) -> Self {
        self.finish_task_summary = Some(summary.into());
        self
    }

    pub fn with_await_user(mut self, request: UserPromptRequest) -> Self {
        self.await_user = Some(request);
        self
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
            file_path: self.file_path,
            evidence_kind: self.evidence_kind,
            evidence_source_path: self.evidence_source_path,
            evidence_summary: self.evidence_summary,
            payload_kind: self.payload_kind,
            invalidate_diagnostic_evidence: self.invalidate_diagnostic_evidence,
            finish_task_summary: self.finish_task_summary,
            await_user: self.await_user,
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
    use crate::tools::{
        BashTool, PatchFileTool, ReadFileTool, ReadMemoryTool, WriteFileTool, WriteMemoryTool,
    };

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

            assert!(
                !obj.contains_key("$schema"),
                "Schema for {} should not contain $schema",
                tool.name()
            );
            assert!(
                !obj.contains_key("title"),
                "Schema for {} should not contain title",
                tool.name()
            );

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
        assert_eq!(envelope.file_path, None);
        assert_eq!(envelope.evidence_kind, None);
        assert_eq!(envelope.evidence_source_path, None);
        assert_eq!(envelope.evidence_summary, None);
        assert!(!envelope.invalidate_diagnostic_evidence);
        assert_eq!(envelope.finish_task_summary, None);
    }
}
