use async_trait::async_trait;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::task;

// Import chromiumoxide for CDP automation
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::handler::Handler;
use chromiumoxide::Page;
use futures::StreamExt;

use crate::tools::{Tool, ToolError, clean_schema};

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
pub struct BrowserActionRequest {
    /// kind: click, type, fill, evaluate...
    pub kind: String,
    /// Target flattened ID from snapshot, e.g., "15"
    pub target_id: Option<String>,
    /// Keyboard text input
    pub text: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
pub struct BrowserToolParams {
    /// Action: status, start, stop, snapshot, act, navigate
    pub action: String,
    
    /// Target URL (for navigate)
    pub target_url: Option<String>,
    
    /// Action details (for act)
    pub request: Option<BrowserActionRequest>,
    
    /// Profile type: 'chrome' (attach) or 'openclaw' (sandbox)
    pub profile: Option<String>,
}

pub struct BrowserState {
    pub browser: Option<Browser>,
    pub active_page: Option<Page>,
}

pub struct BrowserTool {
    state: Arc<RwLock<BrowserState>>,
}

impl BrowserTool {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(BrowserState {
                browser: None,
                active_page: None,
            })),
        }
    }

    async fn handle_start(&self) -> Result<String, ToolError> {
        let mut state = self.state.write().await;
        if state.browser.is_some() {
            return Ok("Browser is already running. Use status to check, or stop to restart.".to_string());
        }

        // Configure sandbox browser
        let config = BrowserConfig::builder()
            .with_head() // Using headed mode for debugging locally. In prod, we may use headless.
            .build()
            .map_err(|e| ToolError::ExecutionFailed(format!("Failed to build browser config: {}", e)))?;

        // Launch the browser process
        let (browser, mut handler) = Browser::launch(config)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("Failed to launch chromium: {}", e)))?;

        // Spawn a background task to process CDP events
        // Without this, the browser connection will stall.
        task::spawn(async move {
            while let Some(h) = handler.next().await {
                if h.is_err() {
                    break;
                }
            }
        });

        // Create an initial page
        let page = browser.new_page("about:blank")
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("Failed to create initial page: {}", e)))?;

        state.browser = Some(browser);
        state.active_page = Some(page);

        Ok("Browser launched successfully and is ready to accept commands.".to_string())
    }

    async fn handle_stop(&self) -> Result<String, ToolError> {
        let mut state = self.state.write().await;
        if state.browser.is_none() {
            return Ok("Browser is already stopped.".to_string());
        }

        // Dropping the page and browser handles will close the connection 
        // and chromiumoxide will kill the child process cleanly.
        state.active_page = None;
        state.browser = None;

        Ok("Browser stopped cleanly.".to_string())
    }

    async fn handle_navigate(&self, url: &str) -> Result<String, ToolError> {
        let state = self.state.read().await;
        let page = state.active_page.as_ref().ok_or_else(|| {
            ToolError::ExecutionFailed("Browser is not running or no active page. Please call 'start' first.".to_string())
        })?;

        // Basic validation
        let target = if url.starts_with("http") { url.to_string() } else { format!("https://{}", url) };

        page.goto(&target)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("Navigation failed: {}", e)))?;

        // Wait for page load
        page.wait_for_navigation()
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("Wait for navigation failed: {}", e)))?;

        Ok(format!("Successfully navigated to {}", target))
    }
}

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> String {
        "browser".to_string()
    }

    fn description(&self) -> String {
        "Control web browser to navigate, extract DOM, and interact with elements. Actions: status, start, stop, navigate, snapshot, act.".to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let schema = schema_for!(BrowserToolParams);
        clean_schema(serde_json::to_value(&schema).unwrap())
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let params: BrowserToolParams = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        match params.action.as_str() {
            "status" => {
                let state = self.state.read().await;
                if state.browser.is_some() {
                    Ok("Browser is running. Call navigate to visit a URL.".to_string())
                } else {
                    Ok("Browser is stopped. Call start to launch it.".to_string())
                }
            },
            "start" => self.handle_start().await,
            "stop" => self.handle_stop().await,
            "navigate" => {
                let url = params.target_url.ok_or_else(|| {
                    ToolError::InvalidArguments("target_url is required for navigate action".to_string())
                })?;
                self.handle_navigate(&url).await
            },
            "snapshot" => {
                let state = self.state.read().await;
                let page = state.active_page.as_ref().ok_or_else(|| {
                    ToolError::ExecutionFailed("Browser is not running. Call start first.".to_string())
                })?;
                
                let js_code = include_str!("browser/extractor.js");
                let result = page.evaluate(js_code).await
                    .map_err(|e| ToolError::ExecutionFailed(format!("Extract failed: {}", e)))?;
                
                if let Some(val) = result.value() {
                    if let Some(s) = val.as_str() {
                        return Ok(s.to_string());
                    }
                }
                Ok("[]".to_string())
            },
            "act" => {
                let state = self.state.read().await;
                let page = state.active_page.as_ref().ok_or_else(|| {
                    ToolError::ExecutionFailed("Browser is not running. Call start first.".to_string())
                })?;

                let req = params.request.ok_or_else(|| {
                    ToolError::InvalidArguments("request field is missing for act action".to_string())
                })?;

                let target_id = req.target_id.ok_or_else(|| {
                    ToolError::InvalidArguments("target_id is required for act action".to_string())
                })?;

                // Fetch coordinates using evaluate
                let get_coords_js = format!(
                    "window.__OC_NODE_MAP__['{}'] ? JSON.stringify(window.__OC_NODE_MAP__['{}']) : null", 
                    target_id, target_id
                );
                
                let coord_res = page.evaluate(get_coords_js).await
                    .map_err(|e| ToolError::ExecutionFailed(format!("Failed to get node coordinates: {}", e)))?;
                
                if let Some(val) = coord_res.value() {
                    if val.is_null() {
                        return Err(ToolError::ExecutionFailed(format!("Target ID {} not found in snapshot", target_id)));
                    }
                    if let Some(s) = val.as_str() {
                        let parsed: serde_json::Value = serde_json::from_str(s).unwrap_or(serde_json::json!({}));
                        if let (Some(x), Some(y)) = (parsed["x"].as_f64(), parsed["y"].as_f64()) {
                            match req.kind.as_str() {
                                "click" => {
                                    // Actually execute click using js fallback since CDP dispatchMouseEvent is lower level
                                    // and requires chromiumoxide advanced mapping. For now, we inject a click payload.
                                    // Phase 4: Use pure CDP coordinates for interaction
                                    use chromiumoxide::layout::Point;
                                    page.click(Point { x, y }).await
                                        .map_err(|e| ToolError::ExecutionFailed(format!("CDP click failed: {}", e)))?;
                                    
                                    // Wait a bit for potential navigation or DOM mutations
                                    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
                                    
                                    return Ok(format!("Successfully clicked on element [{}] at CDP coordinates ({}, {})", target_id, x, y));
                                },
                                "type" => {
                                    // Phase 4: Focus via JS, type via CDP
                                    let focus_js = format!(
                                        "let el = document.querySelector('[data-oc-id=\"{}\"]'); if (el) {{ el.focus(); el.value = ''; true; }} else {{ false; }}",
                                        target_id
                                    );
                                    page.evaluate(focus_js).await
                                        .map_err(|e| ToolError::ExecutionFailed(format!("Focus execution failed: {}", e)))?;
                                    
                                    let text = req.text.unwrap_or_default();
                                    use chromiumoxide::cdp::browser_protocol::input::InsertTextParams;
                                    let _res = page.execute(InsertTextParams::new(text)).await
                                        .map_err(|e| ToolError::ExecutionFailed(format!("CDP insert text failed: {}", e)))?;
                                    
                                    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
                                    return Ok(format!("Successfully typed text into element [{}] via CDP", target_id));
                                },
                                _ => {
                                    return Err(ToolError::InvalidArguments(format!("Unsupported act kind: {}", req.kind)));
                                }
                            }
                        }
                    }
                }
                
                Err(ToolError::ExecutionFailed(format!("Failed to parse coordinates for ID {}", target_id)))
            },
            _ => Err(ToolError::InvalidArguments(format!("Unknown action: {}", params.action))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use crate::tools::Tool;

    #[tokio::test]
    async fn test_browser_lifecycle() {
        let browser_tool = BrowserTool::new();

        // 1. Status - Should be stopped
        let status_res = browser_tool.execute(json!({"action": "status"})).await.unwrap();
        assert!(status_res.contains("stopped"));

        // 2. Start
        let start_res = browser_tool.execute(json!({"action": "start"})).await.unwrap();
        assert!(start_res.contains("ready"));

        // 3. Status - Should be running
        let status_res2 = browser_tool.execute(json!({"action": "status"})).await.unwrap();
        assert!(status_res2.contains("running"));

        // 4. Stop
        let stop_res = browser_tool.execute(json!({"action": "stop"})).await.unwrap();
        assert!(stop_res.contains("stopped"));
    }
}
