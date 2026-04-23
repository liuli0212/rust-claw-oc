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
    /// Optional path to save the content to disk instead of returning it to the LLM.
    pub output_path: Option<String>,
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

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
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
        true
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &crate::tools::ToolContext,
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

        // Sandbox network guard
        if let Some(sandbox) = &ctx.sandbox {
            let policy = sandbox.default_policy();
            sandbox
                .check_network_access(url, policy)
                .map_err(|v| ToolError::ExecutionFailed(v.to_string()))?;
        }

        let output_path = if let Some(path) = &parsed.output_path {
            if let Some(sandbox) = &ctx.sandbox {
                let policy = sandbox.default_policy();
                sandbox
                    .check_path_access(std::path::Path::new(path), true, policy)
                    .map_err(|v| ToolError::ExecutionFailed(v.to_string()))?;
            }
            Some(std::path::PathBuf::from(path))
        } else {
            None
        };

        let max_chars = parsed.max_chars.unwrap_or(12_000).clamp(500, 50_000);

        let mut response = self
            .client
            .get(url)
            .header(reqwest::header::USER_AGENT, "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36")
            .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7")
            .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9,zh-CN;q=0.8,zh;q=0.7")
            .send()
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            tracing::warn!("429 Too Many Requests for {}, retrying in 2s...", url);
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            response = self
                .client
                .get(url)
                .header(reqwest::header::USER_AGENT, "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36")
                .send()
                .await
                .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        }

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

        let mut rendered = raw_body;
        if !parsed.include_html.unwrap_or(false) {
            rendered = html_to_clean_markdown(&rendered);
        }

        let (content, truncated) = if rendered.chars().count() > max_chars {
            (rendered.chars().take(max_chars).collect::<String>(), true)
        } else {
            (rendered, false)
        };

        if let Some(path_buf) = &output_path {
            if let Some(parent) = path_buf.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    ToolError::ExecutionFailed(format!("Failed to create directory: {}", e))
                })?;
            }
            std::fs::write(path_buf, &content).map_err(|e| {
                ToolError::ExecutionFailed(format!(
                    "Failed to write to file {}: {}",
                    path_buf.display(),
                    e
                ))
            })?;

            return serialize_tool_envelope(
                "web_fetch",
                true,
                format!(
                    "Successfully fetched and saved content to {}. Length: {} chars.",
                    path_buf.display(),
                    content.len()
                ),
                Some(0),
                Some(start.elapsed().as_millis()),
                truncated,
            );
        }

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
        "web_search".to_string()
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
        ctx: &crate::tools::protocol::ToolContext,
    ) -> Result<String, ToolError> {
        let start = Instant::now();
        let parsed: TavilySearchArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        if self.api_key.trim().is_empty() {
            return serialize_tool_envelope(
                "web_search",
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

        if let Some(sandbox) = &ctx.sandbox {
            let policy = sandbox.default_policy();
            sandbox
                .check_network_access("https://api.tavily.com/search", policy)
                .map_err(|v| ToolError::ExecutionFailed(v.to_string()))?;
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
                "web_search",
                false,
                format!("Tavily API error: HTTP {} - {}", status, json),
                Some(1),
                Some(start.elapsed().as_millis()),
                false,
            );
        }

        StructuredToolOutput::new(
            "web_search",
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

/// Convert raw HTML to clean Markdown for LLM consumption.
///
/// Pipeline:
/// 1. Strip noise tags (script, style, nav, header, footer, aside, svg, noscript)
/// 2. Convert remaining HTML → Markdown via `fast_html2md`
/// 3. Collapse excessive blank lines
fn html_to_clean_markdown(html: &str) -> String {
    static RE_NOISE: once_cell::sync::Lazy<Vec<regex::Regex>> =
        once_cell::sync::Lazy::new(|| {
            ["script", "style", "nav", "header", "footer", "aside", "svg", "noscript", "iframe"]
                .iter()
                .map(|tag| {
                    regex::Regex::new(&format!(r"(?is)<{tag}\b[^>]*>.*?</{tag}>")).unwrap()
                })
                .collect()
        });

    let mut cleaned = html.to_string();
    for re in RE_NOISE.iter() {
        cleaned = re.replace_all(&cleaned, "").to_string();
    }

    let md = html2md::rewrite_html(&cleaned, false);

    static RE_BLANK_LINES: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"\n{3,}").unwrap());
    static RE_TRAILING_WS: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"[^\S\n]+\n").unwrap());

    let md = RE_BLANK_LINES.replace_all(&md, "\n\n");
    let md = RE_TRAILING_WS.replace_all(&md, "\n");

    md.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::sandbox::{SandboxEnforcer, SandboxLevel, SandboxPolicy};
    use std::sync::Arc;

    #[tokio::test]
    async fn test_web_fetch_tool_rejects_non_http_url() {
        let tool = WebFetchTool::new();
        let err = tool
            .execute(
                serde_json::json!({
                    "url": "ftp://example.com/file.txt"
                }),
                &crate::tools::ToolContext::new("test", "test"),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::InvalidArguments(_)));
        assert!(err.to_string().contains("http:// or https://"));
    }

    #[tokio::test]
    async fn test_web_fetch_tool_blocks_hidden_output_path_in_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let hidden_dir = dir.path().join("hidden");
        let output_path = hidden_dir.join("page.txt");
        std::fs::create_dir_all(&hidden_dir).unwrap();

        let tool = WebFetchTool::new();
        let mut ctx = crate::tools::ToolContext::new("test", "test");
        ctx.sandbox = Some(Arc::new(SandboxEnforcer::disabled_with_policy(
            SandboxPolicy {
                level: SandboxLevel::Restricted,
                allowed_domains: vec!["127.0.0.1".into()],
                hidden_paths: vec![hidden_dir.clone()],
                ..Default::default()
            },
        )));

        let err = tool
            .execute(
                serde_json::json!({
                    "url": "http://127.0.0.1/",
                    "output_path": output_path,
                }),
                &ctx,
            )
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::ExecutionFailed(_)));
        assert!(err.to_string().contains("Sandbox Violation"));
    }

    #[test]
    fn test_html_to_clean_markdown_strips_noise_and_converts() {
        let html = r#"
        <html>
        <head><style>body { color: red; }</style></head>
        <body>
            <nav><a href="/">Home</a><a href="/about">About</a></nav>
            <header><h1>Site Header</h1></header>
            <main>
                <h1>Article Title</h1>
                <p>This is the <strong>main content</strong> of the page.</p>
                <ul><li>Item one</li><li>Item two</li></ul>
                <a href="https://example.com">Example Link</a>
            </main>
            <footer>Copyright 2024</footer>
            <script>alert('evil');</script>
        </body>
        </html>"#;

        let md = html_to_clean_markdown(html);

        // Main content preserved as Markdown
        assert!(md.contains("# Article Title"), "heading missing: {}", md);
        assert!(md.contains("**main content**"), "bold missing: {}", md);
        assert!(md.contains("Item one"), "list missing: {}", md);
        assert!(md.contains("[Example Link]"), "link missing: {}", md);

        // Noise removed
        assert!(!md.contains("alert"), "script not stripped: {}", md);
        assert!(!md.contains("color: red"), "style not stripped: {}", md);
        assert!(!md.contains("Site Header"), "header not stripped: {}", md);
        assert!(!md.contains("Copyright"), "footer not stripped: {}", md);
    }

    #[test]
    fn test_html_to_clean_markdown_handles_plain_text() {
        let plain = "Just some plain text with no HTML tags.";
        let md = html_to_clean_markdown(plain);
        assert_eq!(md, plain);
    }

    #[tokio::test]
    async fn test_web_search_tool_blocks_non_allowlisted_domain_in_sandbox() {
        let tool = TavilySearchTool::new("test-key".to_string());
        let mut ctx = crate::tools::ToolContext::new("test", "test");
        ctx.sandbox = Some(Arc::new(SandboxEnforcer::disabled_with_policy(
            SandboxPolicy {
                level: SandboxLevel::Restricted,
                allowed_domains: vec!["github.com".into()],
                ..Default::default()
            },
        )));

        let err = tool
            .execute(
                serde_json::json!({
                    "query": "sandbox security",
                }),
                &ctx,
            )
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::ExecutionFailed(_)));
        assert!(err.to_string().contains("Sandbox Violation"));
        assert!(err.to_string().contains("api.tavily.com"));
    }
}
