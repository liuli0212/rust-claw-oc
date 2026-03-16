use super::protocol::{clean_schema, serialize_tool_envelope, Tool, ToolError};
use async_trait::async_trait;
use reqwest::Client;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::time::Instant;

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

        let response = self
            .client
            .get(url)
            .header(reqwest::header::USER_AGENT, "rusty-claw/0.1")
            .send()
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        let status = response.status();
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
                    status, url, raw_body
                ),
                Some(1),
                Some(start.elapsed().as_millis()),
                raw_body.len() > 15_000,
            );
        }

        let rendered = raw_body;

        let (content, truncated) = if rendered.chars().count() > max_chars {
            (rendered.chars().take(max_chars).collect::<String>(), true)
        } else {
            (rendered, false)
        };

        serialize_tool_envelope(
            "web_fetch",
            true,
            content,
            Some(0),
            Some(start.elapsed().as_millis()),
            truncated,
        )
    }
}

pub use super::legacy::{TavilySearchArgs, TavilySearchTool};
