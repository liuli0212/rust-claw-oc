use crate::context::{FunctionCall, Message};
use crate::tools::Tool;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Error, Debug)]
pub enum LlmError {
    #[error("Network error: {0}")]
    NetworkError(#[from] reqwest::Error),
    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),
    #[error("API error: {0}")]
    ApiError(String),
}

#[derive(Debug)]
pub enum StreamEvent {
    Text(String),
    Thought(String),
    ToolCall(FunctionCall, Option<String>),
    Error(String),
    Done,
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    fn model_name(&self) -> &str;
    fn provider_name(&self) -> &str;
    fn context_window_size(&self) -> usize;

    async fn generate_text(
        &self,
        messages: Vec<Message>,
        system_instruction: Option<Message>,
    ) -> Result<String, LlmError>;

    async fn stream(
        &self,
        messages: Vec<Message>,
        system_instruction: Option<Message>,
        tools: Vec<Arc<dyn Tool>>,
    ) -> Result<mpsc::Receiver<StreamEvent>, LlmError>;

    async fn generate_structured(
        &self,
        messages: Vec<Message>,
        system_instruction: Option<Message>,
        response_schema: Value,
    ) -> Result<Value, LlmError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiPlatform {
    Gen,
    Vertex,
}
