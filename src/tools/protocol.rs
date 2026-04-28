use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use thiserror::Error;

use crate::delegation::{DelegationBudget, DelegationContext};

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

#[derive(Error, Debug, Clone)]
pub enum ToolError {
    #[error("Execution failed: {0}")]
    ExecutionFailed(String),
    #[error("Invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("Timeout")]
    Timeout,
    #[error("Cancelled: {0}")]
    Cancelled(String),
    #[error("IO error: {0}")]
    IoError(Arc<std::io::Error>),
}

impl From<std::io::Error> for ToolError {
    fn from(err: std::io::Error) -> Self {
        Self::IoError(Arc::new(err))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct ToolTraceContext {
    pub trace_id: String,
    pub run_id: String,
    pub root_session_id: String,
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    pub iteration: Option<u32>,
    pub parent_span_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub session_id: String,
    pub reply_to: String,
    pub visible_tools: Vec<String>,
    pub active_skill_name: Option<String>,
    pub delegation_context: Option<DelegationContext>,
    pub delegation_budget: DelegationBudget,
    pub trace: Option<ToolTraceContext>,
    /// Sandbox enforcer for OS-level and application-level isolation.
    /// `None` means sandbox is disabled (backward compatible).
    pub sandbox: Option<Arc<super::sandbox::SandboxEnforcer>>,
}

impl ToolContext {
    pub fn new(session_id: impl Into<String>, reply_to: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            reply_to: reply_to.into(),
            visible_tools: Vec::new(),
            active_skill_name: None,
            delegation_context: None,
            delegation_budget: DelegationBudget::default(),
            trace: None,
            sandbox: None,
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> String;
    fn description(&self) -> String;
    fn parameters_schema(&self) -> serde_json::Value;
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError>;
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name(),
            description: self.description(),
            input_schema: Some(self.parameters_schema()),
        }
    }
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolResultData {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub tool_name: String,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputSecurity {
    Untrusted,
    Verbatim,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ToolEffects {
    pub recovery_attempted: bool,
    pub recovery_output: Option<String>,
    pub recovery_rule: Option<String>,
    pub file_path: Option<String>,
    pub evidence_kind: Option<String>,
    pub evidence_source_path: Option<String>,
    pub evidence_summary: Option<String>,
    pub payload_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_security: Option<ToolOutputSecurity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_source: Option<String>,
    pub invalidate_diagnostic_evidence: bool,
    /// If set, the tool is requesting that execution pause for user input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub await_user: Option<UserPromptRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StructuredToolOutput {
    #[serde(default, flatten)]
    pub result: ToolResultData,
    #[serde(default, flatten)]
    pub effects: ToolEffects,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ToolExecutionEnvelope {
    #[serde(default, flatten)]
    pub result: ToolResultData,
    #[serde(default, flatten)]
    pub effects: ToolEffects,
}

impl ToolExecutionEnvelope {
    pub fn from_json_str(input: &str) -> Option<Self> {
        serde_json::from_str(input).ok()
    }

    pub fn into_llm_context(mut self) -> Self {
        let source = self
            .effects
            .output_source
            .as_deref()
            .filter(|source| !source.trim().is_empty())
            .unwrap_or_else(|| {
                if self.result.tool_name.trim().is_empty() {
                    "tool"
                } else {
                    self.result.tool_name.as_str()
                }
            });

        self.result.output = match self.effects.output_security {
            Some(ToolOutputSecurity::Untrusted) => {
                crate::security::fence_untrusted(source, &self.result.output)
            }
            Some(ToolOutputSecurity::Verbatim) => {
                crate::security::fence_verbatim(source, &self.result.output)
            }
            None => self.result.output,
        };

        self
    }

    pub fn to_json_string(&self) -> Result<String, ToolError> {
        serde_json::to_string(self).map_err(|e| ToolError::ExecutionFailed(e.to_string()))
    }
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
            result: ToolResultData {
                ok,
                tool_name: tool_name.into(),
                output,
                exit_code,
                duration_ms: duration_ms.map(|value| value.min(u64::MAX as u128) as u64),
                truncated,
            },
            effects: ToolEffects::default(),
        }
    }

    pub fn with_file_path(mut self, path: impl Into<String>) -> Self {
        self.effects.file_path = Some(path.into());
        self
    }

    pub fn with_evidence(
        mut self,
        kind: impl Into<String>,
        source_path: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        self.effects.evidence_kind = Some(kind.into());
        self.effects.evidence_source_path = Some(source_path.into());
        self.effects.evidence_summary = Some(summary.into());
        self
    }

    pub fn with_invalidated_diagnostics(mut self) -> Self {
        self.effects.invalidate_diagnostic_evidence = true;
        self
    }

    pub fn with_payload_kind(mut self, kind: impl Into<String>) -> Self {
        self.effects.payload_kind = Some(kind.into());
        self
    }

    pub fn mark_untrusted(self) -> Self {
        let source = self.result.tool_name.clone();
        self.with_untrusted_output(source)
    }

    pub fn with_untrusted_output(mut self, source: impl Into<String>) -> Self {
        self.effects.output_security = Some(ToolOutputSecurity::Untrusted);
        self.effects.output_source = Some(source.into());
        self
    }

    pub fn mark_verbatim(self) -> Self {
        let source = self.result.tool_name.clone();
        self.with_verbatim_output(source)
    }

    pub fn with_verbatim_output(mut self, source: impl Into<String>) -> Self {
        self.effects.output_security = Some(ToolOutputSecurity::Verbatim);
        self.effects.output_source = Some(source.into());
        self
    }

    pub fn with_await_user(mut self, request: UserPromptRequest) -> Self {
        self.effects.await_user = Some(request);
        self
    }

    pub fn to_json_string(&self) -> Result<String, ToolError> {
        serde_json::to_string(self).map_err(|e| ToolError::ExecutionFailed(e.to_string()))
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

        assert!(envelope.result.ok);
        assert_eq!(envelope.result.tool_name, "write_file");
        assert_eq!(envelope.result.output, "ok");
        assert_eq!(envelope.result.exit_code, Some(0));
        assert_eq!(envelope.result.duration_ms, Some(42));
        assert!(!envelope.result.truncated);
        assert!(!envelope.effects.recovery_attempted);
        assert_eq!(envelope.effects.recovery_output, None);
        assert_eq!(envelope.effects.recovery_rule, None);
        assert_eq!(envelope.effects.file_path, None);
        assert_eq!(envelope.effects.evidence_kind, None);
        assert_eq!(envelope.effects.evidence_source_path, None);
        assert_eq!(envelope.effects.evidence_summary, None);
        assert!(!envelope.effects.invalidate_diagnostic_evidence);
    }
}
