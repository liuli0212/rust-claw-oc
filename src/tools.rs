use async_trait::async_trait;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use regex::Regex;
use reqwest::Client;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::time::timeout;

use crate::utils::truncate_log;
// Helper to clean up JSON schema for strict LLM APIs like Gemini
pub fn clean_schema(mut schema_val: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = schema_val.as_object_mut() {
        obj.remove("$schema");
        obj.remove("title");
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

        tracing::info!("Executing bash via PTY: {}", cmd_str);

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| {
                tracing::error!("BashTool Error: Failed to open PTY - {}", e);
                ToolError::ExecutionFailed(e.to_string())
            })?;

        let mut cmd = CommandBuilder::new("bash");
        cmd.cwd(self.work_dir.clone());
        cmd.arg("-c");
        cmd.arg(&cmd_str);

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| {
                tracing::error!("BashTool Error: Failed to spawn command '{}' - {}", cmd_str, e);
                ToolError::ExecutionFailed(e.to_string())
            })?;
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
                let truncated_stdout = crate::utils::truncate_tool_output(&raw_trimmed);
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
                tracing::warn!(
                    "Bash command timed out after {}s: {}",
                    timeout_secs,
                    cmd_str
                );
                Err(ToolError::Timeout)
            }
        }
    }
}

// Log truncation logic

// Read Memory Tool
pub struct ReadMemoryTool {
    pub workspace: std::sync::Arc<crate::memory::WorkspaceMemory>,
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
    pub workspace: std::sync::Arc<crate::memory::WorkspaceMemory>,
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
    pub store: std::sync::Arc<crate::rag::VectorStore>,
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

// RAG Store Insert Tool
pub struct RagInsertTool {
    pub store: std::sync::Arc<crate::rag::VectorStore>,
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
    /// Explain what changes you are making and why
    pub thought: String,
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
    /// Explain briefly why you need to read this file
    pub thought: String,
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
            Ok(content) => {
                let truncated_content = crate::utils::truncate_tool_output(&content);
                let truncated = truncated_content.len() != content.len();
                serialize_tool_envelope(
                    "read_file",
                    true,
                    truncated_content,
                    None,
                    Some(start.elapsed().as_millis()),
                    truncated,
                )
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

// --- Generic Web Fetch Tool ---
pub struct WebFetchTool {
    pub client: Client,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct WebFetchArgs {
    /// URL to fetch. Must start with http:// or https://
    pub url: String,
    /// Maximum characters to return (default: 12000, clamped to 500..50000).
    pub max_chars: Option<usize>,
    /// Return raw HTML when true. When false, HTML pages are converted to readable text.
    pub include_html: Option<bool>,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }
}

fn decode_common_html_entities(input: &str) -> String {
    input
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn extract_text_from_html(html: &str) -> String {
    let script_re = Regex::new(r"(?is)<script\b[^>]*>.*?</script>").unwrap();
    let style_re = Regex::new(r"(?is)<style\b[^>]*>.*?</style>").unwrap();
    let tag_re = Regex::new(r"(?is)<[^>]+>").unwrap();
    let ws_re = Regex::new(r"[ \t\r\f\v]+").unwrap();
    let nl_re = Regex::new(r"\n{3,}").unwrap();

    let without_script = script_re.replace_all(html, " ");
    let without_style = style_re.replace_all(&without_script, " ");
    let with_line_breaks = without_style
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n")
        .replace("</p>", "\n")
        .replace("</div>", "\n")
        .replace("</li>", "\n")
        .replace("</h1>", "\n")
        .replace("</h2>", "\n")
        .replace("</h3>", "\n")
        .replace("</h4>", "\n")
        .replace("</h5>", "\n")
        .replace("</h6>", "\n");
    let without_tags = tag_re.replace_all(&with_line_breaks, " ");
    let decoded = decode_common_html_entities(&without_tags);
    let normalized_ws = ws_re.replace_all(&decoded, " ");
    let normalized_newlines = normalized_ws.replace(" \n", "\n");
    let squashed_newlines = nl_re.replace_all(&normalized_newlines, "\n\n");
    squashed_newlines.trim().to_string()
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> String {
        "web_fetch".to_string()
    }

    fn description(&self) -> String {
        "Fetches a webpage by URL and returns readable content. Useful for reading specific pages directly."
            .to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schemars::schema_for!(WebFetchArgs)).unwrap())
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let start = Instant::now();
        let parsed: WebFetchArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let url = parsed.url.trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(ToolError::InvalidArguments(
                "url must start with http:// or https://".to_string(),
            ));
        }

        let max_chars = parsed.max_chars.unwrap_or(12_000).clamp(500, 50_000);
        let include_html = parsed.include_html.unwrap_or(false);

        let response = self
            .client
            .get(url)
            .header(reqwest::header::USER_AGENT, "rusty-claw/0.1")
            .send()
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        let status = response.status();
        let final_url = response.url().to_string();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_string();
        let raw_body = response
            .text()
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        if !status.is_success() {
            return serialize_tool_envelope(
                "web_fetch",
                false,
                format!(
                    "Failed to fetch URL. HTTP: {} | URL: {} | Body: {}",
                    status,
                    final_url,
                    truncate_log(&raw_body)
                ),
                Some(1),
                Some(start.elapsed().as_millis()),
                raw_body.len() > 15_000,
            );
        }

        let rendered = if include_html || !content_type.contains("text/html") {
            raw_body
        } else {
            extract_text_from_html(&raw_body)
        };

        let (content, truncated) = if rendered.chars().count() > max_chars {
            (rendered.chars().take(max_chars).collect::<String>(), true)
        } else {
            (rendered, false)
        };

        let output = format!(
            "URL: {}\nFinal URL: {}\nContent-Type: {}\n\n{}",
            url, final_url, content_type, content
        );

        serialize_tool_envelope(
            "web_fetch",
            true,
            output,
            Some(0),
            Some(start.elapsed().as_millis()),
            truncated,
        )
    }
}

// --- Tavily Web Search Tool ---
pub struct TavilySearchTool {
    pub api_key: String,
    pub client: Client,
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

        tracing::info!("TavilySearchTool executing: {}", query);
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
    pub path: PathBuf,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct TaskPlanItem {
    pub step: String,
    pub status: String, // pending, in_progress, completed
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
pub struct TaskPlanState {
    pub items: Vec<TaskPlanItem>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TaskPlanArgs {
    /// Action: get, add, update_status, update_text, remove, clear.
    pub action: String,
    /// For "add", "update_text": The step description.
    pub step: Option<String>,
    /// For "add", "update_status", "update_text": Optional note.
    pub note: Option<String>,
    /// For "update_status", "update_text", "remove": The 0-based index of the item.
    pub index: Option<usize>,
    /// For "update_status": pending, in_progress, completed.
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
}

#[async_trait]
impl Tool for TaskPlanTool {
    fn name(&self) -> String {
        "task_plan".to_string()
    }

    fn description(&self) -> String {
        "Manages the strict execution plan. You MUST update this plan as you progress. Actions: get, add, update_status (index, status), update_text (index, step), remove (index), clear.".to_string()
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
            "add" => {
                let step = parsed.step.ok_or_else(|| {
                    ToolError::InvalidArguments("add requires 'step'".to_string())
                })?;
                let item = TaskPlanItem {
                    step,
                    status: "pending".to_string(),
                    note: parsed.note,
                };
                state.items.push(item);
                self.save_state(&state)?;
            }
            "update_status" => {
                let index = parsed.index.ok_or_else(|| {
                    ToolError::InvalidArguments("update_status requires 'index'".to_string())
                })?;
                if index >= state.items.len() {
                    return Err(ToolError::InvalidArguments(format!("index {} out of bounds", index)));
                }
                let status = parsed.status.ok_or_else(|| {
                    ToolError::InvalidArguments("update_status requires 'status'".to_string())
                })?;
                state.items[index].status = Self::normalize_status(&status)?;
                if let Some(note) = parsed.note {
                    state.items[index].note = Some(note);
                }
                self.save_state(&state)?;
            }
            "update_text" => {
                let index = parsed.index.ok_or_else(|| {
                    ToolError::InvalidArguments("update_text requires 'index'".to_string())
                })?;
                if index >= state.items.len() {
                    return Err(ToolError::InvalidArguments(format!("index {} out of bounds", index)));
                }
                if let Some(step) = parsed.step {
                    state.items[index].step = step;
                }
                if let Some(note) = parsed.note {
                    state.items[index].note = Some(note);
                }
                self.save_state(&state)?;
            }
            "remove" => {
                let index = parsed.index.ok_or_else(|| {
                    ToolError::InvalidArguments("remove requires 'index'".to_string())
                })?;
                if index >= state.items.len() {
                    return Err(ToolError::InvalidArguments(format!("index {} out of bounds", index)));
                }
                state.items.remove(index);
                self.save_state(&state)?;
            }
            _ => {
                return Err(ToolError::InvalidArguments(format!("unsupported action '{}'", parsed.action)));
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


// --- Finish Task Tool ---
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FinishTaskArgs {
    /// A summary of what was accomplished and the final answer to the user
    pub summary: String,
}

pub struct FinishTaskTool;
#[async_trait]
impl Tool for FinishTaskTool {
    fn name(&self) -> String { "finish_task".to_string() }
    fn description(&self) -> String { "Call this tool ONLY when you have fully completed the user's request and have nothing else to do. This will end your execution loop.".to_string() }
    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schemars::schema_for!(FinishTaskArgs)).unwrap())
    }
    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let parsed: FinishTaskArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        Ok(format!("Task marked as finished. Summary: {}", parsed.summary))
    }
}
