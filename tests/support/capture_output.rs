use async_trait::async_trait;
use rusty_claw::core::AgentOutput;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Default)]
pub struct CaptureOutput {
    pub texts: Arc<Mutex<Vec<String>>>,
    pub thoughts: Arc<Mutex<Vec<String>>>,
    pub tool_starts: Arc<Mutex<Vec<(String, String)>>>,
    pub tool_ends: Arc<Mutex<Vec<String>>>,
    pub errors: Arc<Mutex<Vec<String>>>,
}

impl CaptureOutput {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl AgentOutput for CaptureOutput {
    async fn on_text(&self, text: &str) {
        self.texts.lock().await.push(text.to_string());
    }
    async fn on_thinking(&self, text: &str) {
        self.thoughts.lock().await.push(text.to_string());
    }
    async fn on_tool_start(&self, name: &str, args: &str) {
        self.tool_starts.lock().await.push((name.to_string(), args.to_string()));
    }
    async fn on_tool_end(&self, result: &str) {
        self.tool_ends.lock().await.push(result.to_string());
    }
    async fn on_error(&self, error: &str) {
        self.errors.lock().await.push(error.to_string());
    }
}
