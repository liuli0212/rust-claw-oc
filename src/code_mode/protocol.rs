use std::collections::HashMap;

use serde_json::Value;

use super::runtime::value::StoredValue;

#[derive(Debug, Clone)]
pub struct RuntimeTerminalResult {
    pub return_value: Option<Value>,
    pub runtime_error: Option<String>,
    pub cancellation_reason: Option<String>,
    pub stored_values: HashMap<String, StoredValue>,
}

#[derive(Debug)]
pub enum CellCommand {
    Cancel { reason: String },
}

#[derive(Debug, Clone)]
pub struct ToolCallRequestEvent {
    pub seq: u64,
    pub request_id: String,
    pub tool_name: String,
    pub args_json: String,
}

#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    Text {
        seq: u64,
        text: String,
    },
    Notification {
        seq: u64,
        message: String,
    },
    Flush {
        seq: u64,
        value: Option<Value>,
    },
    WaitingForTimer {
        seq: u64,
        resume_after_ms: Option<u64>,
    },
    ToolCallRequested(ToolCallRequestEvent),

    ToolCallDone {
        seq: u64,
        request_id: String,
        ok: bool,
    },
    WorkerCompleted(Result<RuntimeTerminalResult, String>),
}

impl RuntimeEvent {
    pub fn seq(&self) -> Option<u64> {
        match self {
            Self::Text { seq, .. }
            | Self::Notification { seq, .. }
            | Self::Flush { seq, .. }
            | Self::WaitingForTimer { seq, .. }
            | Self::ToolCallDone { seq, .. } => Some(*seq),
            Self::ToolCallRequested(request) => Some(request.seq),
            Self::WorkerCompleted(_) => None,
        }
    }

    pub fn is_visible_in_snapshot(&self) -> bool {
        !matches!(
            self,
            Self::WorkerCompleted(_)
                | Self::WaitingForTimer { .. }
                | Self::ToolCallRequested(_)
                | Self::ToolCallDone { .. }
        )
    }
}

pub fn max_event_seq(events: &[RuntimeEvent]) -> u64 {
    events
        .iter()
        .filter_map(RuntimeEvent::seq)
        .max()
        .unwrap_or(0)
}
