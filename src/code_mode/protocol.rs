use std::collections::HashMap;

use serde_json::Value;

use super::response::ExecRunResult;
use super::runtime;
use super::runtime::value::StoredValue;

pub type RuntimeCellResult = (ExecRunResult, HashMap<String, StoredValue>);

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

    ToolCallResolved {
        seq: u64,
        request_id: String,
        ok: bool,
    },
    Completed {
        seq: u64,
        return_value: Option<Value>,
    },
    Failed {
        seq: u64,
        error: String,
    },
    Cancelled {
        seq: u64,
        reason: String,
    },
    WorkerCompleted(Result<RuntimeCellResult, String>),
    TimerRegistrationChanged {
        seq: u64,
        timer_calls: Vec<runtime::timers::RecordedTimerCall>,
    },
}

impl RuntimeEvent {
    pub fn seq(&self) -> Option<u64> {
        match self {
            Self::Text { seq, .. }
            | Self::Notification { seq, .. }
            | Self::Flush { seq, .. }
            | Self::WaitingForTimer { seq, .. }
            | Self::ToolCallResolved { seq, .. }
            | Self::Completed { seq, .. }
            | Self::Failed { seq, .. }
            | Self::Cancelled { seq, .. }
            | Self::TimerRegistrationChanged { seq, .. } => Some(*seq),
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
                | Self::ToolCallResolved { .. }
                | Self::TimerRegistrationChanged { .. }
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
