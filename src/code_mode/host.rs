use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use super::protocol::RuntimeEvent;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct RuntimeToolRequest {
    pub(crate) cell_id: String,
    pub(crate) seq: u64,
    pub(crate) request_id: String,
    pub(crate) tool_name: String,
    pub(crate) args_json: String,
    pub(crate) outer_tool_call_id: Option<String>,
}

#[async_trait]
pub(crate) trait CellRuntimeHost: Send + Sync {
    fn visible_tool_names(&self) -> Vec<String>;

    fn emit_event(&self, event: RuntimeEvent);

    fn cancellation_reason(&self) -> Option<String>;

    #[allow(dead_code)]
    async fn call_tool(
        &self,
        request: RuntimeToolRequest,
    ) -> Result<String, crate::tools::ToolError>;
}

pub(crate) struct EventBridgeHost {
    visible_tools: Vec<String>,
    event_tx: tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
    cancel_flag: Arc<AtomicBool>,
}

impl EventBridgeHost {
    pub(crate) fn new(
        visible_tools: Vec<String>,
        event_tx: tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
        cancel_flag: Arc<AtomicBool>,
    ) -> Self {
        Self {
            visible_tools,
            event_tx,
            cancel_flag,
        }
    }
}

#[async_trait]
impl CellRuntimeHost for EventBridgeHost {
    fn visible_tool_names(&self) -> Vec<String> {
        self.visible_tools.clone()
    }

    fn emit_event(&self, event: RuntimeEvent) {
        let _ = self.event_tx.send(event);
    }

    fn cancellation_reason(&self) -> Option<String> {
        self.cancel_flag
            .load(Ordering::Acquire)
            .then(|| "Code mode cell execution was cancelled.".to_string())
    }

    async fn call_tool(
        &self,
        request: RuntimeToolRequest,
    ) -> Result<String, crate::tools::ToolError> {
        Err(crate::tools::ToolError::ExecutionFailed(format!(
            "CellRuntimeHost tool execution is not wired for request `{}`.",
            request.request_id
        )))
    }
}
