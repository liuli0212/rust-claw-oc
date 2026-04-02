use super::protocol::{
    clean_schema, serialize_tool_envelope, StructuredToolOutput, Tool, ToolError,
};
use async_trait::async_trait;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Instant;

pub struct PatchFileTool;

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PatchFileArgs {
    /// Explain what changes you are making and why
    pub thought: Option<String>,
    /// Absolute or relative path to the file to edit
    pub path: String,
    /// The unified diff patch content to apply
    pub patch: String,
}

#[async_trait]
impl Tool for PatchFileTool {
    fn name(&self) -> String {
        "patch_file".to_string()
    }

    fn description(&self) -> String {
        "Applies a unified diff patch to a file. This is the preferred way to edit existing files."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        clean_schema(serde_json::to_value(schema_for!(PatchFileArgs)).unwrap())
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &crate::tools::protocol::ToolContext,
    ) -> Result<String, ToolError> {
        let start = Instant::now();
        let parsed: PatchFileArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        // Sandbox path guard
        if let Some(sandbox) = &ctx.sandbox {
            let policy = sandbox.default_policy();
            sandbox
                .check_path_access(std::path::Path::new(&parsed.path), true, policy)
                .map_err(|v| ToolError::ExecutionFailed(v.to_string()))?;
        }

        let patch_path = format!("{}.patch", parsed.path);
        std::fs::write(&patch_path, &parsed.patch).map_err(ToolError::IoError)?;

        let output = std::process::Command::new("patch")
            .arg("-u")
            .arg(&parsed.path)
            .arg("-i")
            .arg(&patch_path)
            .output()
            .map_err(ToolError::IoError)?;

        let _ = std::fs::remove_file(&patch_path);

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let ok = output.status.success();

        if ok {
            StructuredToolOutput::new(
                "patch_file",
                true,
                stdout,
                output.status.code(),
                Some(start.elapsed().as_millis()),
                false,
            )
            .with_file_path(parsed.path)
            .to_json_string()
        } else {
            serialize_tool_envelope(
                "patch_file",
                false,
                format!("STDOUT:\n{}\n\nSTDERR:\n{}", stdout, stderr),
                output.status.code(),
                Some(start.elapsed().as_millis()),
                false,
            )
        }
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
                StructuredToolOutput::new(
                    "read_file",
                    true,
                    truncated_content,
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
pub struct FinishTaskArgs {
    /// A summary of what was accomplished and the final answer to the user
    pub summary: String,
}

pub struct FinishTaskTool {
    pub task_state_store: std::sync::Arc<crate::task_state::TaskStateStore>,
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

#[async_trait]
impl Tool for FinishTaskTool {
    fn name(&self) -> String {
        "finish_task".to_string()
    }

    fn description(&self) -> String {
        "Call this tool ONLY when you have fully completed the user's request and have nothing else to do. This will end your execution loop.".to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schema_for!(FinishTaskArgs)).unwrap())
    }

    fn has_side_effects(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &crate::tools::ToolContext,
    ) -> Result<String, crate::tools::ToolError> {
        let parsed: FinishTaskArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        if let Ok(mut state) = self.task_state_store.load() {
            if state.status == "in_progress" {
                state.status = "completed".to_string();
                let _ = self.task_state_store.save(&state);
            }
        }

        StructuredToolOutput::new(
            "finish_task",
            true,
            format!("Task marked as finished. Summary: {}", parsed.summary),
            Some(0),
            None,
            false,
        )
        .with_finish_task_summary(parsed.summary)
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

    fn cleanup_session(session_id: &str) {
        let session_dir = crate::schema::StoragePaths::session_dir(session_id);
        let _ = std::fs::remove_dir_all(session_dir);
    }

    #[tokio::test]
    async fn test_patch_file_tool() {
        let tool = PatchFileTool;
        let test_file = "test_patch.txt";
        std::fs::write(test_file, "Line 1\nLine 2\nLine 3\n").unwrap();

        let patch = "--- test_patch.txt\n+++ test_patch.txt\n@@ -1,3 +1,3 @@\n Line 1\n-Line 2\n+Line 2 edited\n Line 3\n";
        let args = serde_json::json!({
            "thought": "edit line 2",
            "path": test_file,
            "patch": patch
        });

        let result = tool
            .execute(args, &crate::tools::ToolContext::new("test", "test"))
            .await
            .unwrap();
        assert!(result.contains("true"));

        let content = std::fs::read_to_string(test_file).unwrap();
        assert_eq!(content, "Line 1\nLine 2 edited\nLine 3\n");

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

    #[tokio::test]
    async fn test_finish_task_tool_marks_in_progress_state_completed() {
        let session_id = "test-finish-task-tool";
        cleanup_session(session_id);
        let store = std::sync::Arc::new(crate::task_state::TaskStateStore::new(session_id));
        store
            .save(&crate::task_state::TaskStateSnapshot {
                status: "in_progress".to_string(),
                goal: Some("Ship refactor".to_string()),
                ..crate::task_state::TaskStateSnapshot::empty()
            })
            .unwrap();

        let tool = FinishTaskTool {
            task_state_store: store.clone(),
        };

        let result = tool
            .execute(
                serde_json::json!({
                    "summary": "Refactor complete"
                }),
                &crate::tools::ToolContext::new("test", "test"),
            )
            .await
            .unwrap();
        let updated = store.load().unwrap();

        let envelope: ToolExecutionEnvelope = serde_json::from_str(&result).unwrap();
        assert!(envelope.result.ok);
        assert_eq!(
            envelope.effects.finish_task_summary.as_deref(),
            Some("Refactor complete")
        );
        assert_eq!(updated.status, "completed");
        assert_eq!(updated.goal.as_deref(), Some("Ship refactor"));
        cleanup_session(session_id);
    }
}
