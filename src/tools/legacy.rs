use async_trait::async_trait;
use regex::Regex;
use reqwest::Client;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Instant;
use tokio::time::timeout;
use super::protocol::{
    clean_schema, serialize_tool_envelope, EmptyArgs, Tool, ToolError,
};

// Read Memory Tool
pub struct ReadMemoryTool {
    pub workspace: std::sync::Arc<crate::memory::WorkspaceMemory>,
}

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
        clean_schema(serde_json::to_value(schema_for!(EmptyArgs)).unwrap())
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
        clean_schema(serde_json::to_value(schema_for!(WriteMemoryArgs)).unwrap())
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let start = Instant::now();
        let parsed: WriteMemoryArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        self.workspace.write_memory(&parsed.content).await?;
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
        clean_schema(serde_json::to_value(schema_for!(RagSearchArgs)).unwrap())
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let parsed: RagSearchArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let limit = parsed.limit.unwrap_or(3);

        let results = self
            .store
            .search(&parsed.query, limit)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        if results.is_empty() {
            return Ok(serde_json::json!({"status": "No relevant information found in the knowledge base."}).to_string());
        }

        // Return structured evidence array, LLM understands JSON directly
        // and we fulfill the requirement: "return structured evidence objects instead of text-only tuples"
        Ok(serde_json::to_string_pretty(&results).unwrap_or_else(|_| "[]".to_string()))
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
        clean_schema(serde_json::to_value(schema_for!(RagInsertArgs)).unwrap())
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let parsed: RagInsertArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        self.store
            .insert_chunk(parsed.content, parsed.source)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        Ok("Knowledge successfully embedded and saved into long-term vector memory.".to_string())
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

        serialize_tool_envelope(
            "web_search_tavily",
            true,
            json.to_string(),
            Some(0),
            Some(start.elapsed().as_millis()),
            false,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{
        BashTool, FinishTaskTool, PatchFileTool, ReadFileTool, ToolExecutionEnvelope,
        WebFetchTool, WriteFileTool,
    };
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

        let result = tool.execute(args).await.unwrap();
        assert!(result.contains("true"));

        let content = std::fs::read_to_string(test_file).unwrap();
        assert_eq!(content, "Line 1\nLine 2 edited\nLine 3\n");

        std::fs::remove_file(test_file).unwrap();
    }

    #[test]
    fn test_tool_schema_validation() {
        let workspace = std::sync::Arc::new(crate::memory::WorkspaceMemory::new("test_memory.md"));
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(BashTool::new()),
            Box::new(ReadMemoryTool::new(workspace.clone())),
            Box::new(WriteMemoryTool::new(workspace.clone())),
            Box::new(PatchFileTool),
            Box::new(WriteFileTool),
            Box::new(ReadFileTool),
        ];

        for tool in tools {
            let schema = tool.parameters_schema();
            let obj = schema.as_object().expect("Schema must be an object");

            // Critical checks for OpenAI/Gemini
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
            .execute(serde_json::json!({
                "path": file_path,
                "thought": "inspect large file"
            }))
            .await
            .unwrap();

        let envelope: ToolExecutionEnvelope = serde_json::from_str(&result).unwrap();
        assert!(envelope.ok);
        assert!(envelope.truncated);
        assert!(envelope.output.contains("line-0000"));
        assert!(envelope.output.contains("Truncated"));
    }

    #[tokio::test]
    async fn test_web_fetch_tool_rejects_non_http_url() {
        let tool = WebFetchTool::new();
        let err = tool
            .execute(serde_json::json!({
                "url": "ftp://example.com/file.txt"
            }))
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::InvalidArguments(_)));
        assert!(err.to_string().contains("http:// or https://"));
    }

    #[tokio::test]
    async fn test_send_telegram_message_tool_validates_inputs_before_network() {
        let tool = SendTelegramMessageTool::new("fake-token".to_string());

        let invalid_chat = tool
            .execute(serde_json::json!({
                "chat_id": "abc123",
                "text": "hello"
            }))
            .await
            .unwrap_err();
        assert!(invalid_chat
            .to_string()
            .contains("chat_id must be a numeric Telegram chat ID"));

        let long_text = tool
            .execute(serde_json::json!({
                "chat_id": "12345",
                "text": "x".repeat(4097)
            }))
            .await
            .unwrap_err();
        assert!(long_text
            .to_string()
            .contains("message text exceeds Telegram's 4096 character limit"));
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
            .execute(serde_json::json!({
                "summary": "Refactor complete"
            }))
            .await
            .unwrap();
        let updated = store.load().unwrap();

        assert!(result.contains("Refactor complete"));
        assert_eq!(updated.status, "completed");
        assert_eq!(updated.goal.as_deref(), Some("Ship refactor"));
        cleanup_session(session_id);
    }
}

// --- Telegram Tools ---
pub struct SendTelegramMessageTool {
    pub bot_token: String,
    client: reqwest::Client,
}

impl SendTelegramMessageTool {
    pub fn new(bot_token: String) -> Self {
        Self {
            bot_token,
            client: reqwest::Client::new(),
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SendTelegramMessageArgs {
    /// The target Telegram Chat ID (numeric, e.g., "8578308394")
    pub chat_id: String,
    /// The message text to send (max 4096 chars)
    pub text: String,
}

#[async_trait]
impl Tool for SendTelegramMessageTool {
    fn name(&self) -> String {
        "send_telegram_message".to_string()
    }
    fn description(&self) -> String {
        "Sends a direct Telegram message to a specific chat ID. Useful for notifications from external triggers.".to_string()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schemars::schema_for!(SendTelegramMessageArgs)).unwrap())
    }
    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let parsed: SendTelegramMessageArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        // Validate chat_id is numeric
        if !parsed
            .chat_id
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-')
        {
            return Err(ToolError::InvalidArguments(
                "chat_id must be a numeric Telegram chat ID".to_string(),
            ));
        }

        // Validate text length (Telegram max is 4096)
        if parsed.text.len() > 4096 {
            return Err(ToolError::InvalidArguments(
                "message text exceeds Telegram's 4096 character limit".to_string(),
            ));
        }

        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);
        let resp = self
            .client
            .post(&url)
            .form(&[("chat_id", &parsed.chat_id), ("text", &parsed.text)])
            .send()
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("Network error: {}", e)))?;

        let status = resp.status();
        if status.is_success() {
            Ok(format!(
                "Message successfully sent to Telegram chat ID {}",
                parsed.chat_id
            ))
        } else {
            let err_body = resp.text().await.unwrap_or_default();
            Err(ToolError::ExecutionFailed(format!(
                "Telegram API error ({}): {}",
                status, err_body
            )))
        }
    }
}

// --- LSP Tools ---
pub struct LspGotoDefinitionTool {
    pub lsp_client: std::sync::Arc<crate::lsp::LazyLspClient>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct LspGotoDefinitionArgs {
    /// Path to the file
    pub path: String,
    /// Line number (0-indexed)
    pub line: u32,
    /// Character position (0-indexed)
    pub character: u32,
}

#[async_trait]
impl Tool for LspGotoDefinitionTool {
    fn name(&self) -> String {
        "lsp_goto_definition".to_string()
    }
    fn description(&self) -> String {
        "Go to definition of a symbol using rust-analyzer.".to_string()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schemars::schema_for!(LspGotoDefinitionArgs)).unwrap())
    }
    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let parsed: LspGotoDefinitionArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let path = std::path::PathBuf::from(&parsed.path);
        let client = self
            .lsp_client
            .get_client()
            .await
            .map_err(|e| ToolError::ExecutionFailed(e))?;
        let result = client
            .goto_definition(path, parsed.line, parsed.character)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e))?;

        Ok(serde_json::to_string_pretty(&result).unwrap())
    }
}

// --- LSP Find References Tool ---
pub struct LspFindReferencesTool {
    pub lsp_client: std::sync::Arc<crate::lsp::LazyLspClient>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct LspFindReferencesArgs {
    /// Path to the file
    pub path: String,
    /// Line number (0-indexed)
    pub line: u32,
    /// Character position (0-indexed)
    pub character: u32,
    /// Whether to include the declaration of the symbol
    pub include_declaration: bool,
}

#[async_trait]
impl Tool for LspFindReferencesTool {
    fn name(&self) -> String {
        "lsp_find_references".to_string()
    }
    fn description(&self) -> String {
        "Find all references to a symbol using rust-analyzer.".to_string()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schemars::schema_for!(LspFindReferencesArgs)).unwrap())
    }
    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let parsed: LspFindReferencesArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let path = std::path::PathBuf::from(&parsed.path);
        let client = self
            .lsp_client
            .get_client()
            .await
            .map_err(|e| ToolError::ExecutionFailed(e))?;
        let result = client
            .find_references(
                path,
                parsed.line,
                parsed.character,
                parsed.include_declaration,
            )
            .await
            .map_err(|e| ToolError::ExecutionFailed(e))?;

        Ok(serde_json::to_string_pretty(&result).unwrap())
    }
}

// --- LSP Hover Tool ---
pub struct LspHoverTool {
    pub lsp_client: std::sync::Arc<crate::lsp::LazyLspClient>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct LspHoverArgs {
    /// Path to the file
    pub path: String,
    /// Line number (0-indexed)
    pub line: u32,
    /// Character position (0-indexed)
    pub character: u32,
}

#[async_trait]
impl Tool for LspHoverTool {
    fn name(&self) -> String {
        "lsp_hover".to_string()
    }
    fn description(&self) -> String {
        "Get hover information (types, docs) for a symbol using rust-analyzer.".to_string()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schemars::schema_for!(LspHoverArgs)).unwrap())
    }
    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let parsed: LspHoverArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let path = std::path::PathBuf::from(&parsed.path);
        let client = self
            .lsp_client
            .get_client()
            .await
            .map_err(|e| ToolError::ExecutionFailed(e))?;
        let result = client
            .hover(path, parsed.line, parsed.character)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e))?;

        Ok(serde_json::to_string_pretty(&result).unwrap())
    }
}

// --- LSP Get Diagnostics Tool ---
pub struct LspGetDiagnosticsTool {
    pub lsp_client: std::sync::Arc<crate::lsp::LazyLspClient>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct LspGetDiagnosticsArgs {
    /// Path to the file
    pub path: String,
}

#[async_trait]
impl Tool for LspGetDiagnosticsTool {
    fn name(&self) -> String {
        "lsp_get_diagnostics".to_string()
    }
    fn description(&self) -> String {
        "Get compilation errors and warnings for a file from rust-analyzer.".to_string()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schemars::schema_for!(LspGetDiagnosticsArgs)).unwrap())
    }
    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let parsed: LspGetDiagnosticsArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let path = std::path::PathBuf::from(&parsed.path);
        let client = self
            .lsp_client
            .get_client()
            .await
            .map_err(|e| ToolError::ExecutionFailed(e))?;
        let result = client
            .get_diagnostics(path)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e))?;

        Ok(serde_json::to_string_pretty(&result).unwrap())
    }
}

// --- LSP Get Symbols Tool ---
pub struct LspGetSymbolsTool {
    pub lsp_client: std::sync::Arc<crate::lsp::LazyLspClient>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct LspGetSymbolsArgs {
    /// Path to the file
    pub path: String,
}

#[async_trait]
impl Tool for LspGetSymbolsTool {
    fn name(&self) -> String {
        "lsp_get_symbols".to_string()
    }
    fn description(&self) -> String {
        "Get all symbols (structs, enums, functions) in a file using rust-analyzer.".to_string()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schemars::schema_for!(LspGetSymbolsArgs)).unwrap())
    }
    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let parsed: LspGetSymbolsArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let path = std::path::PathBuf::from(&parsed.path);
        let client = self
            .lsp_client
            .get_client()
            .await
            .map_err(|e| ToolError::ExecutionFailed(e))?;
        let result = client
            .document_symbols(path)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e))?;

        Ok(serde_json::to_string_pretty(&result).unwrap())
    }
}
