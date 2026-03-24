#[cfg(feature = "acp")]
use crate::core::AgentOutput;
#[cfg(feature = "acp")]
use serde::Serialize;
#[cfg(feature = "acp")]
use std::sync::Arc;
#[cfg(feature = "acp")]
use tokio::sync::mpsc;

#[cfg(feature = "acp")]
#[derive(Debug, Serialize)]
#[serde(tag = "type", content = "data")]
pub(super) enum AcpEvent {
    Text(String),
    Thinking(String),
    Error(String),
    Finish { summary: String, status: String },
}

#[cfg(feature = "acp")]
pub(super) struct AcpOutput {
    pub tx: mpsc::UnboundedSender<AcpEvent>,
}

#[cfg(feature = "acp")]
#[async_trait::async_trait]
impl AgentOutput for AcpOutput {
    async fn on_waiting(&self, _message: &str) {}
    fn clear_waiting(&self) {}
    async fn on_text(&self, text: &str) {
        let _ = self.tx.send(AcpEvent::Text(text.to_string()));
    }
    async fn on_thinking(&self, text: &str) {
        let _ = self.tx.send(AcpEvent::Thinking(text.to_string()));
    }
    async fn on_tool_start(&self, _name: &str, _args: &str) {}
    async fn on_tool_end(&self, _result: &str) {}
    async fn on_error(&self, error: &str) {
        let _ = self.tx.send(AcpEvent::Error(error.to_string()));
    }
    async fn on_task_finish(&self, summary: &str) {
        let _ = self.tx.send(AcpEvent::Finish {
            summary: summary.to_string(),
            status: "finished".to_string(),
        });
    }
}

#[cfg(feature = "acp")]
pub(super) struct CancelGuard {
    pub agent: Arc<tokio::sync::Mutex<crate::core::AgentLoop>>,
}

#[cfg(feature = "acp")]
impl Drop for CancelGuard {
    fn drop(&mut self) {
        let agent = self.agent.clone();
        tokio::spawn(async move {
            let agent = agent.lock().await;
            agent.request_cancel();
            tracing::info!("ACP client disconnected, requested agent cancellation");
        });
    }
}
