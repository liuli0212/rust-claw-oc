use async_trait::async_trait;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::task;
use tokio::task::JoinHandle;
use url::Url;

// Import chromiumoxide for CDP automation
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::Page;
use futures::StreamExt;

use crate::tools::{clean_schema, Tool, ToolError};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BrowserMode {
    Launched,
    Attached,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
pub struct BrowserActionRequest {
    /// Interaction kind: "click" or "type"
    pub kind: String,
    /// Numeric element ID from the most recent snapshot, e.g. "5"
    pub target_id: Option<String>,
    /// Text to type (required when kind="type")
    pub text: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
pub struct BrowserToolParams {
    /// One of: "status", "start", "stop", "navigate", "snapshot", "act"
    pub action: String,

    /// URL to navigate to (required for action="navigate"). Auto-prepends https:// if missing.
    pub target_url: Option<String>,

    /// Interaction details (required for action="act"). Must include kind and target_id.
    pub request: Option<BrowserActionRequest>,

    /// "openclaw" (default) launches a sandboxed headless browser.
    /// "chrome" attaches to an existing Chrome instance via CDP.
    pub profile: Option<String>,

    /// CDP endpoint for profile="chrome". Defaults to "http://localhost:9222".
    pub debugging_url: Option<String>,
}

pub struct BrowserState {
    pub browser: Option<Browser>,
    pub active_page: Option<Page>,
    pub(crate) mode: BrowserMode,
    pub(crate) debugging_url: Option<String>,
    handler_handle: Option<JoinHandle<()>>,
}

pub struct BrowserTool {
    state: Arc<RwLock<BrowserState>>,
}

impl Default for BrowserTool {
    fn default() -> Self {
        Self::new()
    }
}

impl BrowserTool {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(BrowserState {
                browser: None,
                active_page: None,
                mode: BrowserMode::Launched,
                debugging_url: None,
                handler_handle: None,
            })),
        }
    }

    fn validate_loopback(raw: &str) -> Result<(), ToolError> {
        let parsed = Url::parse(raw).map_err(|e| {
            ToolError::InvalidArguments(format!("Invalid debugging_url: {}", e))
        })?;
        let host = parsed.host_str().unwrap_or("");
        match host {
            "127.0.0.1" | "localhost" | "::1" | "[::1]" => Ok(()),
            _ => Err(ToolError::InvalidArguments(
                "debugging_url must be a loopback address (127.0.0.1 or localhost)".to_string(),
            )),
        }
    }

    async fn handle_start(
        &self,
        profile: Option<String>,
        debugging_url: Option<String>,
    ) -> Result<String, ToolError> {
        let mut state = self.state.write().await;
        if state.browser.is_some() {
            return Ok(
                "Browser is already running. Use status to check, or stop to restart.".to_string(),
            );
        }

        let profile = profile.as_deref().unwrap_or("openclaw");

        match profile {
            "openclaw" => {
                let config = BrowserConfig::builder()
                    .no_sandbox()
                    .arg("--disable-dev-shm-usage")
                    .build()
                    .map_err(|e| {
                        ToolError::ExecutionFailed(format!(
                            "Failed to build browser config: {}",
                            e
                        ))
                    })?;

                let (browser, mut handler) = Browser::launch(config).await.map_err(|e| {
                    ToolError::ExecutionFailed(format!("Failed to launch chromium: {}", e))
                })?;

                let handle = task::spawn(async move {
                    while let Some(h) = handler.next().await {
                        if h.is_err() {
                            break;
                        }
                    }
                });

                let page = browser.new_page("about:blank").await.map_err(|e| {
                    ToolError::ExecutionFailed(format!("Failed to create initial page: {}", e))
                })?;

                state.browser = Some(browser);
                state.active_page = Some(page);
                state.mode = BrowserMode::Launched;
                state.debugging_url = None;
                state.handler_handle = Some(handle);

                Ok("Browser launched successfully and is ready to accept commands.".to_string())
            }
            "chrome" => {
                // Default to the most common CDP port. Users can override via
                // debugging_url if Chrome is listening on a different port.
                let url = debugging_url
                    .unwrap_or_else(|| "http://localhost:9222".to_string());

                Self::validate_loopback(&url)?;

                let (mut browser, mut handler) =
                    Browser::connect(&url).await.map_err(|e| {
                        ToolError::ExecutionFailed(format!(
                            "Failed to connect to Chrome at {}: {}",
                            url, e
                        ))
                    })?;

                let handle = task::spawn(async move {
                    while let Some(h) = handler.next().await {
                        if h.is_err() {
                            break;
                        }
                    }
                });

                // Wrap remaining fallible operations so the handler is aborted
                // on any failure (fetch_targets, pages, new_page).
                let attach_result = async {
                    // fetch_targets() is required: Browser::connect only tracks targets
                    // created after connection. Without this, existing tabs are invisible.
                    browser.fetch_targets().await.map_err(|e| {
                        ToolError::ExecutionFailed(format!(
                            "Failed to fetch existing targets: {}",
                            e
                        ))
                    })?;

                    // Retry pages() briefly — target attach/page initialization may
                    // still be in progress after fetch_targets returns.
                    let mut found = None;
                    for _ in 0..5 {
                        let pages = browser.pages().await.unwrap_or_default();
                        if let Some(first) = pages.into_iter().next() {
                            found = Some(first);
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                    let page = match found {
                        Some(p) => p,
                        None => browser.new_page("about:blank").await.map_err(|e| {
                            ToolError::ExecutionFailed(format!(
                                "Failed to create initial page: {}",
                                e
                            ))
                        })?,
                    };
                    Ok(page)
                }
                .await;

                match attach_result {
                    Ok(page) => {
                        state.browser = Some(browser);
                        state.active_page = Some(page);
                        state.mode = BrowserMode::Attached;
                        state.debugging_url = Some(url.clone());
                        state.handler_handle = Some(handle);

                        Ok(format!(
                            "Connected to Chrome at {}. Ready to accept commands.",
                            url
                        ))
                    }
                    Err(e) => {
                        handle.abort();
                        Err(e)
                    }
                }
            }
            other => Err(ToolError::InvalidArguments(format!(
                "Unknown profile: '{}'. Use 'openclaw' or 'chrome'.",
                other
            ))),
        }
    }

    async fn handle_stop(&self) -> Result<String, ToolError> {
        let mut state = self.state.write().await;
        if state.browser.is_none() {
            return Ok("Browser is already stopped.".to_string());
        }

        // Abort the CDP event handler task to close the websocket connection.
        // Without this, the task keeps the connection alive after stop.
        if let Some(handle) = state.handler_handle.take() {
            handle.abort();
        }

        // Both modes just null out the handles:
        // - Launched: Browser::drop kills the child process
        // - Attached: Browser::drop is a no-op (no child), so we do NOT call
        //   browser.close() which would send CDP Browser.close and kill user's Chrome
        state.active_page = None;
        state.browser = None;
        state.debugging_url = None;

        Ok("Browser stopped cleanly.".to_string())
    }

    async fn handle_navigate(&self, url: &str) -> Result<String, ToolError> {
        let state = self.state.read().await;
        let page = state.active_page.as_ref().ok_or_else(|| {
            ToolError::ExecutionFailed(
                "Browser is not running or no active page. Please call 'start' first.".to_string(),
            )
        })?;

        // Basic validation
        let target = if url.starts_with("http") {
            url.to_string()
        } else {
            format!("https://{}", url)
        };

        page.goto(&target)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("Navigation failed: {}", e)))?;

        // Wait for page load
        page.wait_for_navigation().await.map_err(|e| {
            ToolError::ExecutionFailed(format!("Wait for navigation failed: {}", e))
        })?;

        Ok(format!("Successfully navigated to {}", target))
    }
}

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> String {
        "browser".to_string()
    }

    fn description(&self) -> String {
        "Control a headless web browser. Typical workflow: \
start → navigate(url) → snapshot(get element IDs) → act(click/type by ID) → snapshot(verify). \
The snapshot action returns a numbered list of interactive elements like `[1] button \"Submit\"`. \
Use the numeric IDs from snapshot as target_id for act. \
Profile \"openclaw\" (default) launches a sandboxed browser. \
Profile \"chrome\" attaches to an existing Chrome via CDP (default endpoint: http://localhost:9222)."
            .to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let schema = schema_for!(BrowserToolParams);
        clean_schema(serde_json::to_value(&schema).unwrap())
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &crate::tools::ToolContext,
    ) -> Result<String, ToolError> {
        let params: BrowserToolParams =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        match params.action.as_str() {
            "status" => {
                let state = self.state.read().await;
                if state.browser.is_some() {
                    match (&state.mode, &state.debugging_url) {
                        (BrowserMode::Attached, Some(url)) => Ok(format!(
                            "Browser is running (attached to {}). Call navigate to visit a URL.",
                            url
                        )),
                        _ => Ok(
                            "Browser is running (launched, headless). Call navigate to visit a URL."
                                .to_string(),
                        ),
                    }
                } else {
                    Ok("Browser is stopped. Call start to launch it.".to_string())
                }
            }
            "start" => self.handle_start(params.profile, params.debugging_url).await,
            "stop" => self.handle_stop().await,
            "navigate" => {
                let url = params.target_url.ok_or_else(|| {
                    ToolError::InvalidArguments(
                        "target_url is required for navigate action".to_string(),
                    )
                })?;
                self.handle_navigate(&url).await
            }
            "snapshot" => {
                let state = self.state.read().await;
                let page = state.active_page.as_ref().ok_or_else(|| {
                    ToolError::ExecutionFailed(
                        "Browser is not running. Call start first.".to_string(),
                    )
                })?;

                let js_code = include_str!("browser/extractor.js");
                let result = page
                    .evaluate(js_code)
                    .await
                    .map_err(|e| ToolError::ExecutionFailed(format!("Extract failed: {}", e)))?;

                if let Some(val) = result.value() {
                    if let Some(s) = val.as_str() {
                        return Ok(s.to_string());
                    }
                }
                Ok("[]".to_string())
            }
            "act" => {
                let state = self.state.read().await;
                let page = state.active_page.as_ref().ok_or_else(|| {
                    ToolError::ExecutionFailed(
                        "Browser is not running. Call start first.".to_string(),
                    )
                })?;

                let req = params.request.ok_or_else(|| {
                    ToolError::InvalidArguments(
                        "request field is missing for act action".to_string(),
                    )
                })?;

                let target_id = req.target_id.ok_or_else(|| {
                    ToolError::InvalidArguments("target_id is required for act action".to_string())
                })?;

                // Fetch coordinates using evaluate
                let get_coords_js = format!(
                    "window.__OC_NODE_MAP__['{}'] ? JSON.stringify(window.__OC_NODE_MAP__['{}']) : null",
                    target_id, target_id
                );

                let coord_res = page.evaluate(get_coords_js).await.map_err(|e| {
                    ToolError::ExecutionFailed(format!("Failed to get node coordinates: {}", e))
                })?;

                if let Some(val) = coord_res.value() {
                    if val.is_null() {
                        return Err(ToolError::ExecutionFailed(format!(
                            "Target ID {} not found in snapshot",
                            target_id
                        )));
                    }
                    if let Some(s) = val.as_str() {
                        let parsed: serde_json::Value =
                            serde_json::from_str(s).unwrap_or(serde_json::json!({}));
                        if let (Some(x), Some(y)) = (parsed["x"].as_f64(), parsed["y"].as_f64()) {
                            match req.kind.as_str() {
                                "click" => {
                                    // Actually execute click using js fallback since CDP dispatchMouseEvent is lower level
                                    // and requires chromiumoxide advanced mapping. For now, we inject a click payload.
                                    // Phase 4: Use pure CDP coordinates for interaction
                                    use chromiumoxide::layout::Point;
                                    page.click(Point { x, y }).await.map_err(|e| {
                                        ToolError::ExecutionFailed(format!(
                                            "CDP click failed: {}",
                                            e
                                        ))
                                    })?;

                                    // Wait a bit for potential navigation or DOM mutations
                                    tokio::time::sleep(tokio::time::Duration::from_millis(150))
                                        .await;

                                    return Ok(format!("Successfully clicked on element [{}] at CDP coordinates ({}, {})", target_id, x, y));
                                }
                                "type" => {
                                    // Phase 4: Focus via JS, type via CDP
                                    let focus_js = format!(
                                        "let el = document.querySelector('[data-oc-id=\"{}\"]'); if (el) {{ el.focus(); el.value = ''; true; }} else {{ false; }}",
                                        target_id
                                    );
                                    page.evaluate(focus_js).await.map_err(|e| {
                                        ToolError::ExecutionFailed(format!(
                                            "Focus execution failed: {}",
                                            e
                                        ))
                                    })?;

                                    let text = req.text.unwrap_or_default();
                                    use chromiumoxide::cdp::browser_protocol::input::InsertTextParams;
                                    let _res = page
                                        .execute(InsertTextParams::new(text))
                                        .await
                                        .map_err(|e| {
                                            ToolError::ExecutionFailed(format!(
                                                "CDP insert text failed: {}",
                                                e
                                            ))
                                        })?;

                                    tokio::time::sleep(tokio::time::Duration::from_millis(150))
                                        .await;
                                    return Ok(format!(
                                        "Successfully typed text into element [{}] via CDP",
                                        target_id
                                    ));
                                }
                                _ => {
                                    return Err(ToolError::InvalidArguments(format!(
                                        "Unsupported act kind: {}",
                                        req.kind
                                    )));
                                }
                            }
                        }
                    }
                }

                Err(ToolError::ExecutionFailed(format!(
                    "Failed to parse coordinates for ID {}",
                    target_id
                )))
            }
            _ => Err(ToolError::InvalidArguments(format!(
                "Unknown action: {}",
                params.action
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;

    fn test_ctx() -> crate::tools::ToolContext {
        crate::tools::ToolContext::new("test", "test")
    }

    async fn start_browser_or_skip(tool: &BrowserTool, label: &str) -> bool {
        match tool.execute(json!({"action": "start"}), &test_ctx()).await {
            Ok(res) => {
                println!("{}", res);
                true
            }
            Err(err) => {
                eprintln!("Skipping {label} because browser start failed: {}", err);
                false
            }
        }
    }

    async fn stop_browser_quietly(tool: &BrowserTool) {
        let _ = tool.execute(json!({"action": "stop"}), &test_ctx()).await;
    }

    #[tokio::test]
    #[serial]
    #[ignore] // requires Chromium and network access
    async fn test_browser_lifecycle() {
        let browser_tool = BrowserTool::new();

        // 1. Status - Should be stopped
        let status_res = browser_tool
            .execute(json!({"action": "status"}), &test_ctx())
            .await
            .unwrap();
        assert!(status_res.contains("stopped"));

        // 2. Start
        let start_res = match browser_tool
            .execute(json!({"action": "start"}), &test_ctx())
            .await
        {
            Ok(res) => res,
            Err(err) => {
                eprintln!(
                    "Skipping browser lifecycle test because start failed: {}",
                    err
                );
                return;
            }
        };
        assert!(start_res.contains("ready"));

        // 3. Status - Should be running
        let status_res2 = browser_tool
            .execute(json!({"action": "status"}), &test_ctx())
            .await
            .unwrap();
        assert!(status_res2.contains("running"));

        // 4. Stop
        let stop_res = browser_tool
            .execute(json!({"action": "stop"}), &test_ctx())
            .await
            .unwrap();
        assert!(stop_res.contains("stopped"));
    }

    #[tokio::test]
    #[serial]
    #[ignore] // requires Chromium and network access
    async fn test_browser_flow() {
        let tool = BrowserTool::new();

        println!("--- Starting Browser ---");
        if !start_browser_or_skip(&tool, "browser flow test").await {
            return;
        }

        println!("--- Navigating ---");
        let res = match tool
            .execute(
                json!({"action": "navigate", "target_url": "https://example.com"}),
                &test_ctx(),
            )
            .await
        {
            Ok(res) => res,
            Err(err) => {
                eprintln!(
                    "Skipping browser flow test due to navigation failure: {}",
                    err
                );
                stop_browser_quietly(&tool).await;
                return;
            }
        };
        println!("{}", res);

        println!("--- Waiting for render ---");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        println!("--- Taking Snapshot ---");
        let res = tool
            .execute(json!({"action": "snapshot"}), &test_ctx())
            .await
            .unwrap();
        println!("Snapshot Result:\n{}", res);

        println!("--- Stopping Browser ---");
        let res = tool
            .execute(json!({"action": "stop"}), &test_ctx())
            .await
            .unwrap();
        println!("{}", res);
    }

    #[tokio::test]
    #[serial]
    #[ignore] // requires Chromium and network access
    async fn test_google_access() {
        let tool = BrowserTool::new();
        println!("--- Starting Browser for Google ---");
        if !start_browser_or_skip(&tool, "google access test").await {
            return;
        }
        println!("--- Navigating to Google ---");
        tool.execute(
            json!({"action": "navigate", "target_url": "https://www.google.com"}),
            &test_ctx(),
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        println!("--- Taking Google Snapshot ---");
        let snapshot = tool
            .execute(json!({"action": "snapshot"}), &test_ctx())
            .await
            .unwrap();
        println!("GOOGLE SNAPSHOT:\n{}", snapshot);
        tool.execute(json!({"action": "stop"}), &test_ctx())
            .await
            .unwrap();
    }

    #[tokio::test]
    #[serial]
    #[ignore] // requires Chromium and network access
    async fn test_google_search_flow() {
        let tool = BrowserTool::new();
        println!("--- Phase 1: Start & Navigate ---");
        if !start_browser_or_skip(&tool, "google search flow test").await {
            return;
        }
        tool.execute(
            json!({"action": "navigate", "target_url": "https://www.google.com"}),
            &test_ctx(),
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        println!("--- Phase 2: Identify Search Box ---");
        let snapshot = tool
            .execute(json!({"action": "snapshot"}), &test_ctx())
            .await
            .unwrap();
        // Look for the input. On Google it's usually an input.
        // Based on previous test, it was [5] or similar.
        println!("Snapshot:\n{}", snapshot);

        // We'll search for 'input' in the snapshot to be dynamic
        let search_id = snapshot
            .lines()
            .find(|l| l.contains("input"))
            .and_then(|l| l.split(']').next())
            .map(|s| s.trim_start_matches('['))
            .unwrap_or("5"); // Fallback to 5 if parsing fails

        println!("--- Phase 3: Typing Search Query ---");
        let res = tool
            .execute(
                json!({
                    "action": "act",
                    "request": {
                        "kind": "type",
                        "target_id": search_id,
                        "text": "OpenClaw github"
                    }
                }),
                &test_ctx(),
            )
            .await
            .unwrap();
        println!("{}", res);

        println!("--- Phase 4: Submit Search ---");
        // We can either find the search button or just press Enter.
        // Our 'act' tool 'type' currently doesn't simulate Enter key easily in the simplified JS fallback
        // unless we add it, but we can just click the 'Google 搜索' button.
        let button_id = snapshot
            .lines()
            .find(|l| l.contains("Google 搜索"))
            .and_then(|l| l.split(']').next())
            .map(|s| s.trim_start_matches('['))
            .unwrap_or("6");

        tool.execute(
            json!({
                "action": "act",
                "request": {
                    "kind": "click",
                    "target_id": button_id
                }
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        println!("--- Phase 5: Waiting for Results ---");
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        println!("--- Phase 6: Result Snapshot ---");
        let results = tool
            .execute(json!({"action": "snapshot"}), &test_ctx())
            .await
            .unwrap();
        println!("SEARCH RESULTS:\n{}", results);

        tool.execute(json!({"action": "stop"}), &test_ctx())
            .await
            .unwrap();
    }

    // --- Unit tests: no external dependencies ---

    #[tokio::test]
    #[serial]
    async fn test_status_text_when_stopped() {
        let tool = BrowserTool::new();
        let res = tool
            .execute(json!({"action": "status"}), &test_ctx())
            .await
            .unwrap();
        assert!(res.contains("stopped"), "Expected 'stopped' in: {}", res);
    }

    #[tokio::test]
    #[serial]
    async fn test_stop_when_already_stopped() {
        let tool = BrowserTool::new();
        let res = tool
            .execute(json!({"action": "stop"}), &test_ctx())
            .await
            .unwrap();
        assert!(
            res.contains("already stopped"),
            "Expected 'already stopped' in: {}",
            res
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_navigate_requires_browser() {
        let tool = BrowserTool::new();
        let res = tool
            .execute(
                json!({"action": "navigate", "target_url": "https://example.com"}),
                &test_ctx(),
            )
            .await;
        assert!(res.is_err());
        let err = res.unwrap_err();
        assert!(
            err.to_string().contains("not running"),
            "Expected 'not running' error: {}",
            err
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_chrome_profile_defaults_debugging_url() {
        let tool = BrowserTool::new();
        // No debugging_url provided — should default to http://localhost:9222
        // and fail with a connection error (no Chrome running in test env).
        let res = tool
            .execute(json!({"action": "start", "profile": "chrome"}), &test_ctx())
            .await;
        assert!(res.is_err());
        let err = res.unwrap_err();
        assert!(
            err.to_string().contains("localhost:9222"),
            "Expected connection error mentioning localhost:9222: {}",
            err
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_chrome_profile_rejects_non_loopback() {
        let tool = BrowserTool::new();
        let res = tool
            .execute(
                json!({
                    "action": "start",
                    "profile": "chrome",
                    "debugging_url": "http://1.2.3.4:9222"
                }),
                &test_ctx(),
            )
            .await;
        assert!(res.is_err());
        let err = res.unwrap_err();
        assert!(
            err.to_string().contains("loopback"),
            "Expected 'loopback' error: {}",
            err
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_loopback_rejects_localhost_subdomain() {
        let tool = BrowserTool::new();
        let res = tool
            .execute(
                json!({
                    "action": "start",
                    "profile": "chrome",
                    "debugging_url": "http://localhost.evil.com:9222"
                }),
                &test_ctx(),
            )
            .await;
        assert!(res.is_err());
        let err = res.unwrap_err();
        assert!(
            err.to_string().contains("loopback"),
            "Expected 'loopback' error for localhost.evil.com: {}",
            err
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_loopback_rejects_userinfo_bypass() {
        let tool = BrowserTool::new();
        let res = tool
            .execute(
                json!({
                    "action": "start",
                    "profile": "chrome",
                    "debugging_url": "http://localhost:9222@evil.example"
                }),
                &test_ctx(),
            )
            .await;
        assert!(res.is_err());
        let err = res.unwrap_err();
        assert!(
            err.to_string().contains("loopback"),
            "Expected 'loopback' error for userinfo bypass: {}",
            err
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_unknown_profile_rejected() {
        let tool = BrowserTool::new();
        let res = tool
            .execute(json!({"action": "start", "profile": "firefox"}), &test_ctx())
            .await;
        assert!(res.is_err());
        let err = res.unwrap_err();
        assert!(
            err.to_string().contains("Unknown profile"),
            "Expected 'Unknown profile' error: {}",
            err
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_snapshot_requires_browser() {
        let tool = BrowserTool::new();
        let res = tool
            .execute(json!({"action": "snapshot"}), &test_ctx())
            .await;
        assert!(res.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_act_requires_browser() {
        let tool = BrowserTool::new();
        let res = tool
            .execute(
                json!({
                    "action": "act",
                    "request": {"kind": "click", "target_id": "1"}
                }),
                &test_ctx(),
            )
            .await;
        assert!(res.is_err());
    }
}
