use super::protocol::{clean_schema, serialize_tool_envelope, EmptyArgs, Tool, ToolError};
use async_trait::async_trait;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;

pub struct ReadMemoryTool {
    pub workspace: Arc<crate::memory::WorkspaceMemory>,
}

impl ReadMemoryTool {
    pub fn new(workspace: Arc<crate::memory::WorkspaceMemory>) -> Self {
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

pub struct WriteMemoryTool {
    pub workspace: Arc<crate::memory::WorkspaceMemory>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct WriteMemoryArgs {
    pub content: String,
}

impl WriteMemoryTool {
    pub fn new(workspace: Arc<crate::memory::WorkspaceMemory>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for WriteMemoryTool {
    fn name(&self) -> String {
        "write_workspace_memory".to_string()
    }

    fn description(&self) -> String {
        "Overwrites the entire workspace long-term memory (MEMORY.md) with new content."
            .to_string()
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

pub struct RagSearchTool {
    pub store: Arc<crate::rag::VectorStore>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RagSearchArgs {
    pub query: String,
    pub limit: Option<usize>,
}

impl RagSearchTool {
    pub fn new(store: Arc<crate::rag::VectorStore>) -> Self {
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
            return Ok(
                serde_json::json!({"status": "No relevant information found in the knowledge base."})
                    .to_string(),
            );
        }

        Ok(serde_json::to_string_pretty(&results).unwrap_or_else(|_| "[]".to_string()))
    }
}

pub struct RagInsertTool {
    pub store: Arc<crate::rag::VectorStore>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RagInsertArgs {
    pub content: String,
    pub source: String,
}

impl RagInsertTool {
    pub fn new(store: Arc<crate::rag::VectorStore>) -> Self {
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
