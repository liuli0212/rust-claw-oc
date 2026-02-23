use async_trait::async_trait;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use std::time::Duration;
use thiserror::Error;

use tokio::time::timeout;
use portable_pty::{CommandBuilder, native_pty_system, PtySize};
use regex::Regex;
use std::io::Read;


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
pub struct BashTool;

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ExecuteCmdArgs {
    /// The shell command to execute
    pub command: String,
    /// Timeout in seconds (default: 30)
    pub timeout: Option<u64>,
}

impl BashTool {
    pub fn new() -> Self {
        Self
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
            if let Some(timeout) = properties.get_mut("timeout").and_then(|t| t.as_object_mut()) {
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

        println!(">> [Executing bash via PTY]: {}", cmd_str);

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }).map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        let mut cmd = CommandBuilder::new("bash");
        cmd.arg("-c");
        cmd.arg(&cmd_str);

        let child = pair.slave.spawn_command(cmd).map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        let child = std::sync::Arc::new(std::sync::Mutex::new(child));
        drop(pair.slave); // Crucial: close slave so master gets EOF

        let mut reader = pair.master.try_clone_reader().map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 { break; }
                if tx.send(buf[..n].to_vec()).is_err() { break; }
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
            }).await.map_err(|e| e.to_string());

            (raw_output, exit_status)
        };

        match timeout(Duration::from_secs(timeout_secs), read_future).await {
            Ok((raw_output, exit_status_res)) => {
                let status_res = exit_status_res.map_err(|e| ToolError::ExecutionFailed(e.to_string()))?
                    .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
                
                // Strip ANSI escape codes
                let re = Regex::new(r"\x1B(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])").unwrap();
                let clean_output = re.replace_all(&raw_output, "").into_owned();
                let clean_output = clean_output.replace("\r\n", "\n");

                let mut res = String::new();
                if !clean_output.trim().is_empty() {
                    res.push_str("OUTPUT:\n");
                    res.push_str(&truncate_log(clean_output.trim()));
                }

                if !status_res.success() {
                    res.push_str(&format!("\nExit code: {}", status_res.exit_code()));
                } else if res.is_empty() {
                    res.push_str("Command executed successfully with no output.");
                }

                Ok(res)
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
    if lines.len() <= 1000 {
        return log.to_string();
    }

    let top = lines[0..500].join("\n");
    let bottom = lines[lines.len() - 500..].join("\n");
    format!(
        "{}\n\n[... Truncated {} lines ...]\n\n{}",
        top,
        lines.len() - 1000,
        bottom
    )
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
            if let Some(timeout) = properties.get_mut("timeout").and_then(|t| t.as_object_mut()) {
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
        let mem = self.workspace.read_memory().await?;
        if mem.is_empty() {
            Ok("Memory is empty.".to_string())
        } else {
            Ok(mem)
        }
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
            if let Some(timeout) = properties.get_mut("timeout").and_then(|t| t.as_object_mut()) {
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
        let parsed_args: WriteMemoryArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        self.workspace.write_memory(&parsed_args.content).await?;
        Ok("Memory updated successfully.".to_string())
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
        
        let results = self.store.search(&parsed_args.query, limit)
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
            
        if results.is_empty() {
            return Ok("No relevant information found in the knowledge base.".to_string());
        }
        
        let mut res = String::new();
        res.push_str("Found the following relevant snippets:\n\n");
        for (content, source, distance) in results {
            res.push_str(&format!("--- Source: {} (Relevance: {:.2}) ---\n{}\n\n", source, distance, content));
        }
        
        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(truncated.contains("line 500"));
        assert!(truncated.contains("[... Truncated 200 lines ...]"));
        assert!(truncated.contains("line 701"));
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

        self.store.insert_chunk(parsed_args.content, parsed_args.source)
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
            
        Ok("Knowledge successfully embedded and saved into long-term vector memory.".to_string())
    }
}
