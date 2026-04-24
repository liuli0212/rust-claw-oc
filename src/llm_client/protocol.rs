use crate::context::{FunctionCall, Message};
use crate::tools::Tool;
use async_trait::async_trait;
use reqwest::Client;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc;

#[allow(clippy::enum_variant_names)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LlmCapabilities {
    pub function_tools: bool,
    pub custom_tools: bool,
    pub parallel_tool_calls: bool,
    pub supports_code_mode: bool,
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    fn model_name(&self) -> &str;
    fn provider_name(&self) -> &str;
    fn context_window(&self) -> usize {
        crate::llm_client::policy::estimate_context_window(self.model_name())
    }
    fn capabilities(&self) -> LlmCapabilities;

    async fn stream(
        &self,
        messages: Vec<Message>,
        system_instruction: Option<Message>,
        tools: Vec<Arc<dyn Tool>>,
    ) -> Result<mpsc::Receiver<StreamEvent>, LlmError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiPlatform {
    Gen,
    Vertex,
}

pub(super) fn create_standard_client(base_url: Option<&str>) -> Client {
    let mut builder = Client::builder()
        .connect_timeout(std::time::Duration::from_secs(300))
        .timeout(std::time::Duration::from_secs(600))
        .pool_idle_timeout(std::time::Duration::from_secs(600))
        .pool_max_idle_per_host(10)
        .tcp_keepalive(Some(std::time::Duration::from_secs(30)))
        .http2_keep_alive_interval(Some(std::time::Duration::from_secs(15)))
        .http2_keep_alive_timeout(std::time::Duration::from_secs(20))
        .http2_keep_alive_while_idle(true)
        .http2_initial_stream_window_size(4 * 1024 * 1024)
        .http2_initial_connection_window_size(4 * 1024 * 1024)
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                "X-Server-Timeout",
                reqwest::header::HeaderValue::from_static("600"),
            );
            headers.insert(
                "x-goog-api-client",
                reqwest::header::HeaderValue::from_static("rusty-claw/0.1.0"),
            );
            headers
        })
        .gzip(true);

    if let Some(url) = base_url {
        let no_proxy = std::env::var("no_proxy")
            .or_else(|_| std::env::var("NO_PROXY"))
            .unwrap_or_default();

        let bypass = no_proxy.split(',').any(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return false;
            }
            if entry == "*" {
                return true;
            }

            url.contains(entry)
        });

        if bypass {
            tracing::debug!("Bypassing proxy for URL: {} (matched in NO_PROXY)", url);
            builder = builder.no_proxy();
        }
    }

    builder.build().unwrap_or_else(|_| Client::new())
}
