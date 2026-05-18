#[cfg(feature = "acp")]
use crate::core::AgentOutput;
#[cfg(feature = "acp")]
use crate::session_manager::SessionManager;
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
    ToolStart { name: String, args: String },
    ToolEnd { result: String },
    PlanUpdate { summary: String, status: String },
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
    async fn on_tool_start(&self, name: &str, args: &str) {
        let _ = self.tx.send(AcpEvent::ToolStart {
            name: name.to_string(),
            args: args.to_string(),
        });
    }
    async fn on_tool_end(&self, result: &str) {
        let _ = self.tx.send(AcpEvent::ToolEnd {
            result: result.to_string(),
        });
    }
    async fn on_error(&self, error: &str) {
        let _ = self.tx.send(AcpEvent::Error(error.to_string()));
    }
    async fn on_plan_update(&self, state: &crate::task_state::TaskStateSnapshot) {
        let _ = self.tx.send(AcpEvent::PlanUpdate {
            summary: state.summary(),
            status: state.status.clone(),
        });
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
    pub session_manager: Arc<SessionManager>,
    pub session_id: String,
}

#[cfg(feature = "acp")]
impl Drop for CancelGuard {
    fn drop(&mut self) {
        let session_manager = self.session_manager.clone();
        let session_id = self.session_id.clone();
        tokio::spawn(async move {
            session_manager.cancel_session(&session_id).await;
            tracing::info!("ACP client disconnected, requested cancellation");
        });
    }
}
