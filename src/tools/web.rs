use super::protocol::{
    clean_schema, serialize_tool_envelope, StructuredToolOutput, Tool, ToolError,
};
use async_trait::async_trait;
use reqwest::Client;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Instant;

pub struct WebFetchTool {
    pub client: Client,
}

pub struct TavilySearchTool {
    pub api_key: String,
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

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TavilySearchArgs {
    pub query: String,
    pub max_results: Option<usize>,
    pub topic: Option<String>,
    pub search_depth: Option<String>,
    pub include_answer: Option<bool>,
    pub include_raw_content: Option<bool>,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }
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

    fn has_side_effects(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &crate::tools::ToolContext,
    ) -> Result<String, crate::tools::ToolError> {
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

        StructuredToolOutput::new(
            "web_fetch",
            true,
            content,
            Some(0),
            Some(start.elapsed().as_millis()),
            truncated,
        )
        .with_payload_kind("web_content")
        .to_json_string()
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

    fn parameters_schema(&self) -> Value {
        clean_schema(serde_json::to_value(schemars::schema_for!(TavilySearchArgs)).unwrap())
    }

    fn has_side_effects(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &crate::tools::protocol::ToolContext,
    ) -> Result<String, ToolError> {
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

        StructuredToolOutput::new(
            "web_search_tavily",
            true,
            json.to_string(),
            Some(0),
            Some(start.elapsed().as_millis()),
            false,
        )
        .with_payload_kind("web_search")
        .to_json_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_web_fetch_tool_rejects_non_http_url() {
        let tool = WebFetchTool::new();
        let err = tool
            .execute(
                serde_json::json!({
                    "url": "ftp://example.com/file.txt"
                }),
                &crate::tools::ToolContext {
                    session_id: "test".into(),
                    reply_to: "test".into(),
                },
            )
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::InvalidArguments(_)));
        assert!(err.to_string().contains("http:// or https://"));
    }
}
