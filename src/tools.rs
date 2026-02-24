use async_trait::async_trait;
use reqwest::Client;
use schemars::{schema_for, JsonSchema};

// Helper to clean up JSON schema for strict LLM APIs like Gemini
pub fn clean_schema(mut schema_val: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = schema_val.as_object_mut() {
        obj.remove("$schema");
        obj.remove("title");
    }
    schema_val
}

use serde::{Deserialize, Serialize};
use serde_json::Value;

use std::time::Duration;
use std::time::Instant;
use thiserror::Error;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use regex::Regex;
use std::io::Read;
use std::path::PathBuf;
use tokio::time::timeout;

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

// Bash Tool
pub struct BashTool {
    work_dir: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ExecuteCmdArgs {
    /// The shell command to execute
    pub command: String,
    /// Timeout in seconds (default: 30)
    pub timeout: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct BashExecutionResult {
    pub ok: bool,
    pub command: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub duration_ms: u128,
    pub truncated: bool,
}

#[derive(Serialize, Deserialize, Debug)]
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

fn serialize_tool_envelope(
    tool_name: &str,
    ok: bool,
    output: String,
    exit_code: Option<i32>,
    duration_ms: Option<u128>,
    truncated: bool,
) -> Result<String, ToolError> {
    let envelope = ToolExecutionEnvelope {
        ok,
        tool_name: tool_name.to_string(),
        output,
        exit_code,
        duration_ms,
        truncated,
        recovery_attempted: false,
        recovery_output: None,
        recovery_rule: None,
    };
    serde_json::to_string(&envelope).map_err(|e| ToolError::ExecutionFailed(e.to_string()))
}

impl BashTool {
    pub fn new() -> Self {
        let work_dir = std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .to_string_lossy()
            .to_string();
        Self { work_dir }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> String {
        "execute_bash".to_string()
    }

    fn description(&self) -> String {
        "Executes a bash command. Returns stdout and stderr. Use carefully.".to_string()
    }

    fn parameters_schema(&self) -> Value {
        let schema = schema_for!(ExecuteCmdArgs);
        let mut val = serde_json::to_value(&schema).unwrap();
        val.as_object_mut().unwrap().remove("$schema");
        val.as_object_mut().unwrap().remove("title");
        if let Some(properties) = val.get_mut("properties").and_then(|p| p.as_object_mut()) {
            if let Some(timeout) = properties
                .get_mut("timeout")
                .and_then(|t| t.as_object_mut())
            {
                if let Some(type_arr) = timeout.get("type").and_then(|t| t.as_array()) {
                    if let Some(first) = type_arr.first() {
                        let f = first.clone();
                        timeout.insert("type".to_string(), f);
                    }
                }
            }
        }
        val
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let parsed_args: ExecuteCmdArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let timeout_secs = parsed_args.timeout.unwrap_or(30);
        let cmd_str = parsed_args.command;
        let start = Instant::now();

        println!(">> [Executing bash via PTY]: {}", cmd_str);

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        let mut cmd = CommandBuilder::new("bash");
        cmd.cwd(self.work_dir.clone());
        cmd.arg("-c");
        cmd.arg(&cmd_str);

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        let child = std::sync::Arc::new(std::sync::Mutex::new(child));
        drop(pair.slave); // Crucial: close slave so master gets EOF

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });

        let child_clone = child.clone();
        let read_future = async move {
            let mut raw_output = String::new();
            while let Some(chunk) = rx.recv().await {
                raw_output.push_str(&String::from_utf8_lossy(&chunk));
            }

            let exit_status = tokio::task::spawn_blocking(move || {
                let mut c = child_clone.lock().unwrap();
                c.wait()
            })
            .await
            .map_err(|e| e.to_string());

            (raw_output, exit_status)
        };

        match timeout(Duration::from_secs(timeout_secs), read_future).await {
            Ok((raw_output, exit_status_res)) => {
                let status_res = exit_status_res
                    .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?
                    .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

                // Strip ANSI escape codes
                let re = Regex::new(r"\x1B(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])").unwrap();
                let clean_output = re.replace_all(&raw_output, "").into_owned();
                let clean_output = clean_output.replace("\r\n", "\n");
                let raw_trimmed = clean_output.trim().to_string();
                let truncated_stdout = truncate_log(&raw_trimmed);
                let truncated = truncated_stdout != raw_trimmed;
                let result = BashExecutionResult {
                    ok: status_res.success(),
                    command: cmd_str.clone(),
                    stdout: truncated_stdout,
                    stderr: String::new(),
                    exit_code: i32::try_from(status_res.exit_code()).unwrap_or(i32::MAX),
                    duration_ms: start.elapsed().as_millis(),
                    truncated,
                };
                let output = if result.stderr.trim().is_empty() {
                    result.stdout.clone()
                } else if result.stdout.trim().is_empty() {
                    result.stderr.clone()
                } else {
                    format!("{}\n{}", result.stdout, result.stderr)
                };
                serialize_tool_envelope(
                    "execute_bash",
                    result.ok,
                    output,
                    Some(result.exit_code),
                    Some(result.duration_ms),
                    result.truncated,
                )
            }
            Err(_) => {
                let mut c = child.lock().unwrap();
                let _ = c.kill();
                Err(ToolError::Timeout)
            }
        }
    }
}

// Log truncation logic
fn truncate_log(log: &str) -> String {
    let lines: Vec<&str> = log.lines().collect();
    let max_lines = 200;

    // First pass: line-based truncation
    let truncated_str = if lines.len() <= max_lines {
        log.to_string()
    } else {
        let head = lines[0..100].join("\n");
        let tail = lines[lines.len() - 100..].join("\n");
        format!(
            "{}\n\n[... Truncated {} lines ...]\n\n{}",
            head,
            lines.len() - max_lines,
            tail
        )
    };

    // Second pass: character-based truncation (e.g., max 15000 chars)
    let max_chars = 15000;
    if truncated_str.len() <= max_chars {
        truncated_str
    } else {
        let head: String = truncated_str.chars().take(max_chars / 2).collect();
        let tail: String = truncated_str
            .chars()
            .skip(truncated_str.len() - (max_chars / 2))
            .collect();
        format!(
            "{}\n\n[... Truncated {} characters ...]\n\n{}",
            head,
            truncated_str.len() - max_chars,
            tail
        )
    }
}

// Read Memory Tool
pub struct ReadMemoryTool {
    workspace: std::sync::Arc<crate::memory::WorkspaceMemory>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct EmptyArgs {}

impl ReadMemoryTool {
    pub fn new(workspace: std::sync::Arc<crate::memory::WorkspaceMemory>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for ReadMemoryTool {
    fn name(&self) -> String {
        "read_workspace_memory".to_string()
    }

    fn description(&self) -> String {
        "Reads the long-term workspace memory (MEMORY.md).".to_string()
    }

    fn parameters_schema(&self) -> Value {
        let schema = schema_for!(EmptyArgs);
        let mut val = serde_json::to_value(&schema).unwrap();
        val.as_object_mut().unwrap().remove("$schema");
        val.as_object_mut().unwrap().remove("title");
        if let Some(properties) = val.get_mut("properties").and_then(|p| p.as_object_mut()) {
            if let Some(timeout) = properties
                .get_mut("timeout")
                .and_then(|t| t.as_object_mut())
            {
                if let Some(type_arr) = timeout.get("type").and_then(|t| t.as_array()) {
                    if let Some(first) = type_arr.first() {
                        let f = first.clone();
                        timeout.insert("type".to_string(), f);
                    }
                }
            }
        }
        val
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let start = Instant::now();
        let mem = self.workspace.read_memory().await?;
        let output = if mem.is_empty() {
            "Memory is empty.".to_string()
        } else {
            mem
        };
        serialize_tool_envelope(
            "read_workspace_memory",
            true,
            output,
            None,
            Some(start.elapsed().as_millis()),
            false,
        )
    }
}

// Write Memory Tool
pub struct WriteMemoryTool {
    workspace: std::sync::Arc<crate::memory::WorkspaceMemory>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct WriteMemoryArgs {
    /// The complete content to save as memory. This overwrites the old memory.
    pub content: String,
}

impl WriteMemoryTool {
    pub fn new(workspace: std::sync::Arc<crate::memory::WorkspaceMemory>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for WriteMemoryTool {
    fn name(&self) -> String {
        "write_workspace_memory".to_string()
    }

    fn description(&self) -> String {
        "Overwrites the entire workspace long-term memory (MEMORY.md) with new content.".to_string()
    }

    fn parameters_schema(&self) -> Value {
        let schema = schema_for!(WriteMemoryArgs);
        let mut val = serde_json::to_value(&schema).unwrap();
        val.as_object_mut().unwrap().remove("$schema");
        val.as_object_mut().unwrap().remove("title");
        if let Some(properties) = val.get_mut("properties").and_then(|p| p.as_object_mut()) {
            if let Some(timeout) = properties
                .get_mut("timeout")
                .and_then(|t| t.as_object_mut())
            {
                if let Some(type_arr) = timeout.get("type").and_then(|t| t.as_array()) {
                    if let Some(first) = type_arr.first() {
                        let f = first.clone();
                        timeout.insert("type".to_string(), f);
                    }
                }
            }
        }
        val
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let start = Instant::now();
        let parsed_args: WriteMemoryArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        self.workspace.write_memory(&parsed_args.content).await?;
        serialize_tool_envelope(
            "write_workspace_memory",
            true,
            "Memory updated successfully.".to_string(),
            None,
            Some(start.elapsed().as_millis()),
            false,
        )
    }
}

// RAG Store Tool
pub struct RagSearchTool {
    store: std::sync::Arc<crate::rag::VectorStore>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RagSearchArgs {
    /// The semantic query to search for in the long-term knowledge base.
    pub query: String,
    /// Maximum number of related snippets to return.
    pub limit: Option<usize>,
}

impl RagSearchTool {
    pub fn new(store: std::sync::Arc<crate::rag::VectorStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for RagSearchTool {
    fn name(&self) -> String {
        "search_knowledge_base".to_string()
    }

    fn description(&self) -> String {
        "Semantically searches the long-term knowledge base for related information using vector embeddings. Use this when you need to recall past lessons, code snippets, or project guidelines.".to_string()
    }

    fn parameters_schema(&self) -> Value {
        let schema = schema_for!(RagSearchArgs);
        let mut val = serde_json::to_value(&schema).unwrap();
        val.as_object_mut().unwrap().remove("$schema");
        val.as_object_mut().unwrap().remove("title");
        if let Some(properties) = val.get_mut("properties").and_then(|p| p.as_object_mut()) {
            if let Some(limit) = properties.get_mut("limit").and_then(|t| t.as_object_mut()) {
                if let Some(type_arr) = limit.get("type").and_then(|t| t.as_array()) {
                    if let Some(first) = type_arr.first() {
                        let f = first.clone();
                        limit.insert("type".to_string(), f);
                    }
                }
            }
        }
        val
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let parsed_args: RagSearchArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let limit = parsed_args.limit.unwrap_or(3);

        let results = self
            .store
            .search(&parsed_args.query, limit)
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        if results.is_empty() {
            return Ok("No relevant information found in the knowledge base.".to_string());
        }

        let mut res = String::new();
        res.push_str("Found the following relevant snippets:\n\n");
        for (content, source, distance) in results {
            res.push_str(&format!(
                "--- Source: {} (Relevance: {:.2}) ---\n{}\n\n",
                source, distance, content
            ));
        }

        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[test]
    fn test_truncate_log_short() {
        let log = "line 1\nline 2\nline 3";
        assert_eq!(truncate_log(log), log);
    }

    #[test]
    fn test_truncate_log_long() {
        let mut log = String::new();
        for i in 1..=1200 {
            log.push_str(&format!("line {}\n", i));
        }
        let truncated = truncate_log(&log);
        assert!(truncated.contains("line 1"));
        assert!(truncated.contains("line 100"));
        assert!(truncated.contains("[... Truncated 1000 lines ...]"));
        assert!(truncated.contains("line 1101"));
        assert!(truncated.contains("line 1200"));
        assert!(!truncated.contains("line 600"));
    }

    #[test]
    fn test_bash_tool_schema() {
        let tool = BashTool::new();
        let schema = tool.parameters_schema();
        let props = schema.get("properties").unwrap();
        assert!(props.get("command").is_some());
        assert!(props.get("timeout").is_some());

        // Ensure no $schema or title (Gemini bug workaround)
        assert!(schema.get("$schema").is_none());
        assert!(schema.get("title").is_none());
    }

    #[tokio::test]
    async fn test_regression_file_tools_roundtrip() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("hello.txt");
        let content = "hello regression";

        let writer = WriteFileTool;
        let write_res = writer
            .execute(serde_json::json!({
                "path": file_path.to_string_lossy().to_string(),
                "content": content
            }))
            .await
            .unwrap();
        let write_env: ToolExecutionEnvelope = serde_json::from_str(&write_res).unwrap();
        assert!(write_env.ok);
        assert!(write_env.output.contains("Successfully wrote"));

        let reader = ReadFileTool;
        let read_res = reader
            .execute(serde_json::json!({
                "path": file_path.to_string_lossy().to_string()
            }))
            .await
            .unwrap();
        let read_env: ToolExecutionEnvelope = serde_json::from_str(&read_res).unwrap();
        assert!(read_env.ok);
        assert_eq!(read_env.output, content);
    }

    #[tokio::test]
    async fn test_regression_workspace_memory_tools_roundtrip() {
        let dir = tempdir().unwrap();
        let workspace = Arc::new(crate::memory::WorkspaceMemory::new(
            dir.path().to_str().unwrap(),
        ));

        let write_tool = WriteMemoryTool::new(workspace.clone());
        let write_res = write_tool
            .execute(serde_json::json!({
                "content": "remember this"
            }))
            .await
            .unwrap();
        let write_env: ToolExecutionEnvelope = serde_json::from_str(&write_res).unwrap();
        assert!(write_env.ok);
        assert_eq!(write_env.output, "Memory updated successfully.");

        let read_tool = ReadMemoryTool::new(workspace);
        let read_res = read_tool.execute(serde_json::json!({})).await.unwrap();
        let read_env: ToolExecutionEnvelope = serde_json::from_str(&read_res).unwrap();
        assert!(read_env.ok);
        assert_eq!(read_env.output, "remember this");
    }

    #[tokio::test]
    async fn test_regression_bash_tool_smoke() {
        let tool = BashTool::new();
        let result = tool
            .execute(serde_json::json!({
                "command": "printf 'regression-ok'",
                "timeout": 5
            }))
            .await
            .unwrap();

        let parsed: ToolExecutionEnvelope = serde_json::from_str(&result).unwrap();
        assert!(parsed.ok);
        assert_eq!(parsed.exit_code, Some(0));
        assert!(parsed.output.contains("regression-ok"));
    }

    #[tokio::test]
    async fn test_regression_task_plan_lifecycle() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("task-plan.json");
        let tool = TaskPlanTool::new(path);

        let replace = tool
            .execute(serde_json::json!({
                "action": "replace",
                "items": [
                    { "step": "Design", "status": "completed" },
                    { "step": "Implement", "status": "in_progress" }
                ]
            }))
            .await
            .unwrap();
        let replace_env: ToolExecutionEnvelope = serde_json::from_str(&replace).unwrap();
        assert!(replace_env.ok);

        let update = tool
            .execute(serde_json::json!({
                "action": "update_status",
                "index": 1,
                "status": "completed"
            }))
            .await
            .unwrap();
        let update_env: ToolExecutionEnvelope = serde_json::from_str(&update).unwrap();
        assert!(update_env.ok);
        assert!(update_env.output.contains("\"completed\""));

        let get = tool
            .execute(serde_json::json!({ "action": "get" }))
            .await
            .unwrap();
        let get_env: ToolExecutionEnvelope = serde_json::from_str(&get).unwrap();
        assert!(get_env.ok);
        assert!(get_env.output.contains("\"Design\""));
        assert!(get_env.output.contains("\"Implement\""));
    }

    #[tokio::test]
    async fn test_regression_tavily_tool_without_key_returns_error_envelope() {
        let tool = TavilySearchTool::new(String::new());
        let result = tool
            .execute(serde_json::json!({
                "query": "rust tokio tutorial"
            }))
            .await
            .unwrap();
        let env: ToolExecutionEnvelope = serde_json::from_str(&result).unwrap();
        assert!(!env.ok);
        assert!(env.output.contains("TAVILY_API_KEY"));
    }
}

// RAG Store Insert Tool
pub struct RagInsertTool {
    store: std::sync::Arc<crate::rag::VectorStore>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RagInsertArgs {
    /// The knowledge or code snippet to save into the long-term semantic memory.
    pub content: String,
    /// A brief description or file path indicating where this knowledge came from.
    pub source: String,
}

impl RagInsertTool {
    pub fn new(store: std::sync::Arc<crate::rag::VectorStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for RagInsertTool {
    fn name(&self) -> String {
        "memorize_knowledge".to_string()
    }

    fn description(&self) -> String {
        "Saves important concepts, code patterns, or project lessons into the vector knowledge base so it can be semantically retrieved in the future.".to_string()
    }

    fn parameters_schema(&self) -> Value {
        let schema = schema_for!(RagInsertArgs);
        let mut val = serde_json::to_value(&schema).unwrap();
        val.as_object_mut().unwrap().remove("$schema");
        val.as_object_mut().unwrap().remove("title");
        val
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let parsed_args: RagInsertArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        self.store
            .insert_chunk(parsed_args.content, parsed_args.source)
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        Ok("Knowledge successfully embedded and saved into long-term vector memory.".to_string())
    }
}

// --- File Write Tool ---
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct WriteFileArgs {
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
        "Writes complete content to a specified file. Overwrites if exists. Very reliable for writing code.".to_string()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schemars::schema_for!(WriteFileArgs)).unwrap())
    }
    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let start = Instant::now();
        let parsed: WriteFileArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        if let Some(parent) = std::path::Path::new(&parsed.path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&parsed.path, &parsed.content) {
            Ok(_) => serialize_tool_envelope(
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
            ),
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

// --- File Read Tool ---
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ReadFileArgs {
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
        clean_schema(serde_json::to_value(schemars::schema_for!(ReadFileArgs)).unwrap())
    }
    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let start = Instant::now();
        let parsed: ReadFileArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        match std::fs::read_to_string(&parsed.path) {
            Ok(content) => serialize_tool_envelope(
                "read_file",
                true,
                content,
                None,
                Some(start.elapsed().as_millis()),
                false,
            ),
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

// --- Tavily Web Search Tool ---
pub struct TavilySearchTool {
    api_key: String,
    client: Client,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TavilySearchArgs {
    /// The search query.
    pub query: String,
    /// Maximum number of results to return (default: 5, max: 10 recommended).
    pub max_results: Option<usize>,
    /// Search topic: "general" or "news".
    pub topic: Option<String>,
    /// Search depth: "basic" or "advanced".
    pub search_depth: Option<String>,
    /// Include model-generated direct answer in response.
    pub include_answer: Option<bool>,
    /// Include larger raw content snippets.
    pub include_raw_content: Option<bool>,
}

impl TavilySearchTool {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: Client::new(),
        }
    }
}

#[async_trait]
impl Tool for TavilySearchTool {
    fn name(&self) -> String {
        "web_search_tavily".to_string()
    }

    fn description(&self) -> String {
        "Searches the web via Tavily and returns concise results with source URLs.".to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schemars::schema_for!(TavilySearchArgs)).unwrap())
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let start = Instant::now();
        let parsed: TavilySearchArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        if self.api_key.trim().is_empty() {
            return serialize_tool_envelope(
                "web_search_tavily",
                false,
                "TAVILY_API_KEY is not configured.".to_string(),
                Some(1),
                Some(start.elapsed().as_millis()),
                false,
            );
        }

        let query = parsed.query.trim();
        if query.is_empty() {
            return Err(ToolError::InvalidArguments(
                "query cannot be empty".to_string(),
            ));
        }

        let max_results = parsed.max_results.unwrap_or(5).clamp(1, 10);
        let mut payload = serde_json::json!({
            "api_key": self.api_key,
            "query": query,
            "max_results": max_results,
            "include_answer": parsed.include_answer.unwrap_or(true),
            "include_raw_content": parsed.include_raw_content.unwrap_or(false),
        });
        if let Some(topic) = parsed.topic {
            payload["topic"] = serde_json::Value::String(topic);
        }
        if let Some(depth) = parsed.search_depth {
            payload["search_depth"] = serde_json::Value::String(depth);
        }

        let response = self
            .client
            .post("https://api.tavily.com/search")
            .json(&payload)
            .send()
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        let status = response.status();
        let json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        if !status.is_success() {
            return serialize_tool_envelope(
                "web_search_tavily",
                false,
                format!("Tavily API error: HTTP {} - {}", status, json),
                Some(1),
                Some(start.elapsed().as_millis()),
                false,
            );
        }

        let mut out = String::new();
        if let Some(answer) = json.get("answer").and_then(|v| v.as_str()) {
            if !answer.trim().is_empty() {
                out.push_str("Answer:\n");
                out.push_str(answer.trim());
                out.push_str("\n\n");
            }
        }

        if let Some(results) = json.get("results").and_then(|v| v.as_array()) {
            out.push_str("Sources:\n");
            for (i, item) in results.iter().enumerate() {
                let title = item
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Untitled");
                let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
                let content = item
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                out.push_str(&format!("{}. {} ({})\n", i + 1, title, url));
                if !content.is_empty() {
                    out.push_str(&format!(
                        "   {}\n",
                        truncate_log(content).replace('\n', " ")
                    ));
                }
            }
        }

        if out.trim().is_empty() {
            out = json.to_string();
        }

        serialize_tool_envelope(
            "web_search_tavily",
            true,
            out,
            Some(0),
            Some(start.elapsed().as_millis()),
            false,
        )
    }
}

// --- Task Plan Tool ---
pub struct TaskPlanTool {
    path: PathBuf,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct TaskPlanItem {
    /// A short plan step.
    pub step: String,
    /// One of: pending, in_progress, completed.
    pub status: String,
    /// Optional note/details for this step.
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
pub struct TaskPlanState {
    pub items: Vec<TaskPlanItem>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TaskPlanArgs {
    /// Action: get, replace, add, update_status, remove, clear.
    pub action: String,
    /// Used by replace.
    pub items: Option<Vec<TaskPlanItem>>,
    /// Used by add.
    pub item: Option<TaskPlanItem>,
    /// Used by update_status/remove.
    pub index: Option<usize>,
    /// Used by update_status.
    pub status: Option<String>,
}

impl TaskPlanTool {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn load_state(&self) -> Result<TaskPlanState, ToolError> {
        if !self.path.exists() {
            return Ok(TaskPlanState::default());
        }
        let raw = std::fs::read_to_string(&self.path).map_err(ToolError::IoError)?;
        serde_json::from_str::<TaskPlanState>(&raw).map_err(|e| {
            ToolError::ExecutionFailed(format!("Failed to parse task plan state: {}", e))
        })
    }

    fn save_state(&self, state: &TaskPlanState) -> Result<(), ToolError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(ToolError::IoError)?;
        }
        let raw = serde_json::to_string_pretty(state)
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        std::fs::write(&self.path, raw).map_err(ToolError::IoError)
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

    fn validate_single_in_progress(items: &[TaskPlanItem]) -> Result<(), ToolError> {
        let count = items
            .iter()
            .filter(|i| i.status.as_str() == "in_progress")
            .count();
        if count > 1 {
            return Err(ToolError::InvalidArguments(
                "at most one plan item can be in_progress".to_string(),
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl Tool for TaskPlanTool {
    fn name(&self) -> String {
        "task_plan".to_string()
    }

    fn description(&self) -> String {
        "Manages a persistent local task plan. Supports actions: get, replace, add, update_status, remove, clear.".to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schemars::schema_for!(TaskPlanArgs)).unwrap())
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let start = Instant::now();
        let parsed: TaskPlanArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let action = parsed.action.trim().to_lowercase();
        let mut state = self.load_state()?;

        match action.as_str() {
            "get" => {}
            "clear" => {
                state.items.clear();
                self.save_state(&state)?;
            }
            "replace" => {
                let mut items = parsed.items.ok_or_else(|| {
                    ToolError::InvalidArguments("replace requires 'items'".to_string())
                })?;
                for item in &mut items {
                    item.status = Self::normalize_status(&item.status)?;
                }
                Self::validate_single_in_progress(&items)?;
                state.items = items;
                self.save_state(&state)?;
            }
            "add" => {
                let mut item = parsed.item.ok_or_else(|| {
                    ToolError::InvalidArguments("add requires 'item'".to_string())
                })?;
                item.status = Self::normalize_status(&item.status)?;
                state.items.push(item);
                Self::validate_single_in_progress(&state.items)?;
                self.save_state(&state)?;
            }
            "update_status" => {
                let index = parsed.index.ok_or_else(|| {
                    ToolError::InvalidArguments("update_status requires 'index'".to_string())
                })?;
                if index >= state.items.len() {
                    return Err(ToolError::InvalidArguments(format!(
                        "index {} out of bounds (len={})",
                        index,
                        state.items.len()
                    )));
                }
                let status = parsed.status.ok_or_else(|| {
                    ToolError::InvalidArguments("update_status requires 'status'".to_string())
                })?;
                state.items[index].status = Self::normalize_status(&status)?;
                Self::validate_single_in_progress(&state.items)?;
                self.save_state(&state)?;
            }
            "remove" => {
                let index = parsed.index.ok_or_else(|| {
                    ToolError::InvalidArguments("remove requires 'index'".to_string())
                })?;
                if index >= state.items.len() {
                    return Err(ToolError::InvalidArguments(format!(
                        "index {} out of bounds (len={})",
                        index,
                        state.items.len()
                    )));
                }
                state.items.remove(index);
                self.save_state(&state)?;
            }
            _ => {
                return Err(ToolError::InvalidArguments(format!(
                    "unsupported action '{}'",
                    parsed.action
                )));
            }
        }

        let output = serde_json::to_string_pretty(&state)
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        serialize_tool_envelope(
            "task_plan",
            true,
            output,
            Some(0),
            Some(start.elapsed().as_millis()),
            false,
        )
    }
}
