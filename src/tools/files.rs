use super::protocol::{
    clean_schema, serialize_tool_envelope, StructuredToolOutput, Tool, ToolError,
};
use async_trait::async_trait;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};

use std::time::Instant;

pub struct PatchFileTool;

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FileEdit {
    /// The exact text to find in the file. Must be unique.
    pub search: String,
    /// The text to replace it with.
    pub replace: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PatchFileArgs {
    /// Explain what changes you are making and why
    pub thought: Option<String>,
    /// Absolute or relative path to the file to edit
    pub path: String,
    /// List of edits to apply to the file sequentially.
    pub edits: Vec<FileEdit>,
}

#[async_trait]
impl Tool for PatchFileTool {
    fn name(&self) -> String {
        "patch_file".to_string()
    }

    fn description(&self) -> String {
        "Replaces exact text blocks in a file. This is the preferred way to edit existing files. Provide enough context in `search` to make it unique. You can provide multiple edits."
            .to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schema_for!(PatchFileArgs)).unwrap())
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &crate::tools::ToolContext,
    ) -> Result<String, crate::tools::ToolError> {
        let start = std::time::Instant::now();
        let parsed: PatchFileArgs = serde_json::from_value(args)
            .map_err(|e| crate::tools::ToolError::InvalidArguments(e.to_string()))?;

        // Sandbox path guard
        if let Some(sandbox) = &ctx.sandbox {
            let policy = sandbox.default_policy();
            sandbox
                .check_path_access(std::path::Path::new(&parsed.path), true, policy)
                .map_err(|v| crate::tools::ToolError::ExecutionFailed(v.to_string()))?;
        }

        let mut file_content = std::fs::read_to_string(&parsed.path)
            .map_err(|e| crate::tools::ToolError::IoError(std::sync::Arc::new(e)))?;

        // Validate all edits first (Atomicity)
        for (i, edit) in parsed.edits.iter().enumerate() {
            let mut search_str = edit.search.as_str();
            let mut replace_str = edit.replace.as_str();
            let mut matches: Vec<_> = file_content.match_indices(search_str).collect();

            // Fallback: Fuzzy match (trim leading/trailing whitespace)
            if matches.is_empty() {
                let trimmed_search = edit.search.trim();
                if !trimmed_search.is_empty() {
                    let fuzzy_matches: Vec<_> =
                        file_content.match_indices(trimmed_search).collect();
                    if fuzzy_matches.len() == 1 {
                        search_str = trimmed_search;
                        replace_str = edit.replace.trim();
                        matches = fuzzy_matches;
                    }
                }
            }

            if matches.is_empty() {
                return crate::tools::protocol::StructuredToolOutput::new(
                    "patch_file",
                    false,
                    format!("Edit {} failed: Search text not found in file. Exact and fuzzy match failed. Please ensure you provide the exact text.", i + 1),
                    Some(1),
                    Some(start.elapsed().as_millis()),
                    false,
                ).to_json_string();
            } else if matches.len() > 1 {
                return crate::tools::protocol::StructuredToolOutput::new(
                    "patch_file",
                    false,
                    format!("Edit {} failed: Search text found multiple times in file. Please provide more context lines to make it unique.", i + 1),
                    Some(1),
                    Some(start.elapsed().as_millis()),
                    false,
                ).to_json_string();
            }
            // Apply in memory
            file_content = file_content.replace(search_str, replace_str);
        }

        std::fs::write(&parsed.path, &file_content)
            .map_err(|e| crate::tools::ToolError::IoError(std::sync::Arc::new(e)))?;

        crate::tools::protocol::StructuredToolOutput::new(
            "patch_file",
            true,
            format!(
                "Successfully applied {} edits to {}",
                parsed.edits.len(),
                parsed.path
            ),
            Some(0),
            Some(start.elapsed().as_millis()),
            false,
        )
        .with_invalidated_diagnostics()
        .with_file_path(parsed.path)
        .to_json_string()
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct WriteFileArgs {
    /// Explain what changes you are making and why
    pub thought: Option<String>,
    /// Absolute or relative path to the file to write
    pub path: String,
    /// The complete content to write into the file
    pub content: String,
}

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> String {
        "write_file".to_string()
    }

    fn description(&self) -> String {
        "Writes complete content to a specified file. Overwrites if exists. Very reliable for writing code."
            .to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schema_for!(WriteFileArgs)).unwrap())
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &crate::tools::ToolContext,
    ) -> Result<String, crate::tools::ToolError> {
        let start = Instant::now();
        let parsed: WriteFileArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        // Sandbox path guard
        if let Some(sandbox) = &ctx.sandbox {
            let policy = sandbox.default_policy();
            sandbox
                .check_path_access(std::path::Path::new(&parsed.path), true, policy)
                .map_err(|v| ToolError::ExecutionFailed(v.to_string()))?;
        }

        if let Some(parent) = std::path::Path::new(&parsed.path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        match std::fs::write(&parsed.path, &parsed.content) {
            Ok(_) => StructuredToolOutput::new(
                "write_file",
                true,
                format!(
                    "Successfully wrote {} bytes to {}",
                    parsed.content.len(),
                    parsed.path
                ),
                None,
                Some(start.elapsed().as_millis()),
                false,
            )
            .with_invalidated_diagnostics()
            .with_file_path(parsed.path)
            .to_json_string(),
            Err(e) => serialize_tool_envelope(
                "write_file",
                false,
                format!("Failed to write {}: {}", parsed.path, e),
                Some(1),
                Some(start.elapsed().as_millis()),
                false,
            ),
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    /// Explain briefly why you need to read this file
    pub thought: Option<String>,
    /// Path to the file to read
    pub path: String,
}

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> String {
        "read_file".to_string()
    }

    fn description(&self) -> String {
        "Reads the exact contents of a file from disk.".to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schema_for!(ReadFileArgs)).unwrap())
    }

    fn has_side_effects(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &crate::tools::ToolContext,
    ) -> Result<String, crate::tools::ToolError> {
        let start = Instant::now();
        let parsed: ReadFileArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        if let Some(sandbox) = &ctx.sandbox {
            let policy = sandbox.default_policy();
            sandbox
                .check_path_access(std::path::Path::new(&parsed.path), false, policy)
                .map_err(|v| ToolError::ExecutionFailed(v.to_string()))?;
        }

        match std::fs::read_to_string(&parsed.path) {
            Ok(content) => {
                let truncated_content = crate::utils::truncate_tool_output(&content);
                let truncated = truncated_content.len() != content.len();
                let fenced = crate::security::fence_verbatim("read_file", &truncated_content);
                StructuredToolOutput::new(
                    "read_file",
                    true,
                    fenced,
                    None,
                    Some(start.elapsed().as_millis()),
                    truncated,
                )
                .with_evidence(
                    "file",
                    parsed.path.clone(),
                    format!("Direct read of {}", parsed.path),
                )
                .to_json_string()
            }
            Err(e) => serialize_tool_envelope(
                "read_file",
                false,
                format!("Failed to read {}: {}", parsed.path, e),
                Some(1),
                Some(start.elapsed().as_millis()),
                false,
            ),
        }
    }
}

pub struct TaskPlanTool {
    #[allow(dead_code)]
    pub session_id: String,
    pub task_state_store: std::sync::Arc<crate::task_state::TaskStateStore>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TaskPlanArgs {
    /// Action: get, add, update_status, update_text, update_goal, remove, clear.
    pub action: String,
    /// For "add", "update_text": The step description.
    pub step: Option<String>,
    /// For "update_goal": The new concise goal description.
    pub goal: Option<String>,
    /// For "add", "update_status", "update_text": Optional note.
    pub note: Option<String>,
    /// For "update_status", "update_text", "remove": The 0-based index of the item.
    pub index: Option<usize>,
    /// For "update_status": pending, in_progress, completed.
    pub status: Option<String>,
}

impl TaskPlanTool {
    pub fn new(
        session_id: String,
        task_state_store: std::sync::Arc<crate::task_state::TaskStateStore>,
    ) -> Self {
        Self {
            session_id,
            task_state_store,
        }
    }

    fn normalize_status(status: &str) -> Result<String, ToolError> {
        let normalized = status.trim().to_lowercase();
        match normalized.as_str() {
            "pending" | "in_progress" | "completed" => Ok(normalized),
            _ => Err(ToolError::InvalidArguments(format!(
                "invalid status '{}'; expected pending|in_progress|completed",
                status
            ))),
        }
    }
}

#[async_trait]
impl Tool for TaskPlanTool {
    fn name(&self) -> String {
        "task_plan".to_string()
    }

    fn description(&self) -> String {
        "Manages the strict execution plan. You MUST update this plan as you progress. Actions: get, add, update_status (index, status), update_text (index, step), update_goal (goal), remove (index), clear. If the task completely changes, use update_goal to set a new concise goal.".to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schema_for!(TaskPlanArgs)).unwrap())
    }

    fn has_side_effects(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &crate::tools::ToolContext,
    ) -> Result<String, crate::tools::ToolError> {
        let start = Instant::now();
        let parsed: TaskPlanArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let action = parsed.action.trim().to_lowercase();

        let mut state = self
            .task_state_store
            .load()
            .unwrap_or_else(|_| crate::task_state::TaskStateSnapshot::empty());

        if state.task_id.is_none() {
            state.task_id = Some(format!("tsk_{}", uuid::Uuid::new_v4().simple()));
            state.status = "in_progress".to_string();
        }

        match action.as_str() {
            "get" => {}
            "clear" => state.plan_steps.clear(),
            "add" => {
                let step = parsed.step.ok_or_else(|| {
                    ToolError::InvalidArguments("add requires 'step'".to_string())
                })?;
                state.plan_steps.push(crate::task_state::PlanStep {
                    step,
                    status: "pending".to_string(),
                    note: None,
                });
            }
            "update_status" => {
                let index = parsed.index.ok_or_else(|| {
                    ToolError::InvalidArguments("update_status requires 'index'".to_string())
                })?;
                let status = parsed.status.ok_or_else(|| {
                    ToolError::InvalidArguments("update_status requires 'status'".to_string())
                })?;
                if state.plan_steps.is_empty() {
                    return Err(ToolError::ExecutionFailed(
                        "No tasks planed yet.".to_string(),
                    ));
                }
                let normalized_status = Self::normalize_status(&status)?;

                if index < state.plan_steps.len() {
                    state.plan_steps[index].status = normalized_status;
                    if let Some(note) = parsed.note {
                        state.plan_steps[index].note = Some(note);
                    }
                } else {
                    return Err(ToolError::ExecutionFailed(format!(
                        "Index {} out of bounds",
                        index
                    )));
                }
            }
            "update_text" => {
                let index = parsed.index.ok_or_else(|| {
                    ToolError::InvalidArguments("update_text requires 'index'".to_string())
                })?;

                if state.plan_steps.is_empty() {
                    return Err(ToolError::ExecutionFailed(
                        "No tasks planed yet.".to_string(),
                    ));
                }
                if index < state.plan_steps.len() {
                    if let Some(step) = parsed.step {
                        state.plan_steps[index].step = step;
                    }
                    if let Some(note) = parsed.note {
                        state.plan_steps[index].note = Some(note);
                    }
                } else {
                    return Err(ToolError::ExecutionFailed(format!(
                        "Index {} out of bounds",
                        index
                    )));
                }
            }
            "update_goal" => {
                let new_goal = parsed.goal.ok_or_else(|| {
                    ToolError::InvalidArguments("update_goal requires 'goal'".to_string())
                })?;
                state.goal = Some(new_goal);
            }
            "remove" => {
                let index = parsed.index.ok_or_else(|| {
                    ToolError::InvalidArguments("remove requires 'index'".to_string())
                })?;
                if index < state.plan_steps.len() {
                    state.plan_steps.remove(index);
                } else {
                    return Err(ToolError::ExecutionFailed(format!(
                        "Index {} out of bounds",
                        index
                    )));
                }
            }
            _ => {
                return Err(ToolError::InvalidArguments(format!(
                    "unsupported action '{}'",
                    parsed.action
                )));
            }
        }

        let _ = self.task_state_store.save(&state);

        let output = if let Ok(state) = self.task_state_store.load() {
            state.summary()
        } else {
            "Plan updated.".to_string()
        };

        StructuredToolOutput::new(
            "task_plan",
            true,
            output,
            Some(0),
            Some(start.elapsed().as_millis()),
            false,
        )
        .with_payload_kind("plan")
        .to_json_string()
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SendFileArgs {
    /// Explain what file you are sending and why
    pub thought: Option<String>,
    /// Absolute or relative path to the file to send
    pub path: String,
}

pub struct SendFileTool;

#[async_trait]
impl Tool for SendFileTool {
    fn name(&self) -> String {
        "send_file".to_string()
    }

    fn description(&self) -> String {
        "Sends a file (image, document, audio, etc.) to the user's chat.".to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schema_for!(SendFileArgs)).unwrap())
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &crate::tools::ToolContext,
    ) -> Result<String, crate::tools::ToolError> {
        let parsed: SendFileArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        if !std::path::Path::new(&parsed.path).exists() {
            return Err(ToolError::ExecutionFailed(format!(
                "File not found: {}",
                parsed.path
            )));
        }

        StructuredToolOutput::new(
            "send_file",
            true,
            format!("File {} sent to user.", parsed.path),
            Some(0),
            None,
            false,
        )
        .with_file_path(parsed.path)
        .to_json_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::protocol::ToolExecutionEnvelope;
    use crate::tools::sandbox::{SandboxEnforcer, SandboxLevel, SandboxPolicy};
    use std::sync::Arc;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_patch_file_tool() {
        let tool = PatchFileTool;
        let test_file = "test_patch.txt";
        std::fs::write(test_file, "Line 1\nLine 2\nLine 3\n").unwrap();

        let args = serde_json::json!({
            "thought": "edit line 2 and 3",
            "path": test_file,
            "edits": [
                {
                    "search": "Line 2\n",
                    "replace": "Line 2 edited\n"
                },
                {
                    "search": "Line 3\n",
                    "replace": "Line 3 edited\n"
                }
            ]
        });

        let result = tool
            .execute(args, &crate::tools::ToolContext::new("test", "test"))
            .await
            .unwrap();
        assert!(result.contains("true"));

        let content = std::fs::read_to_string(test_file).unwrap();
        assert_eq!(content, "Line 1\nLine 2 edited\nLine 3 edited\n");

        // Test fuzzy match fallback
        let fuzzy_args = serde_json::json!({
            "thought": "fuzzy match test",
            "path": test_file,
            "edits": [
                {
                    "search": "  Line 2 edited  \n",
                    "replace": "  Line 2 fuzzy  \n"
                }
            ]
        });

        let fuzzy_result = tool
            .execute(fuzzy_args, &crate::tools::ToolContext::new("test", "test"))
            .await
            .unwrap();
        assert!(fuzzy_result.contains("true"));

        let fuzzy_content = std::fs::read_to_string(test_file).unwrap();
        assert_eq!(fuzzy_content, "Line 1\nLine 2 fuzzy\nLine 3 edited\n");

        std::fs::remove_file(test_file).unwrap();
    }

    #[tokio::test]
    async fn test_read_file_tool_marks_large_output_as_truncated() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("large.txt");
        let large_content = (0..3000)
            .map(|idx| format!("line-{idx:04}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&file_path, &large_content).unwrap();

        let tool = ReadFileTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "path": file_path,
                    "thought": "inspect large file"
                }),
                &crate::tools::ToolContext::new("test", "test"),
            )
            .await
            .unwrap();

        let envelope: ToolExecutionEnvelope = serde_json::from_str(&result).unwrap();
        assert!(envelope.result.ok);
        assert!(envelope.result.truncated);
        assert!(envelope.result.output.contains("line-0000"));
        assert!(envelope.result.output.contains("Truncated"));
    }

    #[tokio::test]
    async fn test_read_file_tool_blocks_hidden_path_in_sandbox() {
        let dir = tempdir().unwrap();
        let hidden_dir = dir.path().join("hidden");
        let file_path = hidden_dir.join("secret.txt");
        std::fs::create_dir_all(&hidden_dir).unwrap();
        std::fs::write(&file_path, "top-secret").unwrap();

        let tool = ReadFileTool;
        let mut ctx = crate::tools::ToolContext::new("test", "test");
        ctx.sandbox = Some(Arc::new(SandboxEnforcer::disabled_with_policy(
            SandboxPolicy {
                level: SandboxLevel::Restricted,
                hidden_paths: vec![hidden_dir.clone()],
                ..Default::default()
            },
        )));

        let err = tool
            .execute(
                serde_json::json!({
                    "path": file_path,
                    "thought": "inspect hidden file"
                }),
                &ctx,
            )
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::ExecutionFailed(_)));
        assert!(err.to_string().contains("Sandbox Violation"));
    }
}
