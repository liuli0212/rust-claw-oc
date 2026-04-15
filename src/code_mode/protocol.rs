use std::collections::HashMap;

use serde_json::Value;

use super::response::ExecRunResult;
use super::runtime;
use super::runtime::value::StoredValue;

pub type RuntimeCellResult = (ExecRunResult, HashMap<String, StoredValue>);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrainRequest {
    pub wait_for_event: bool,
    pub wait_timeout_ms: Option<u64>,
    pub refresh_slice_ms: Option<u64>,
}

impl DrainRequest {
    pub fn to_completion() -> Self {
        Self {
            wait_for_event: true,
            wait_timeout_ms: None,
            refresh_slice_ms: None,
        }
    }

    pub fn wait_for_next_event() -> Self {
        Self::for_wait(None, None)
    }

    pub fn for_wait(wait_timeout_ms: Option<u64>, refresh_slice_ms: Option<u64>) -> Self {
        Self {
            wait_for_event: true,
            wait_timeout_ms,
            refresh_slice_ms,
        }
    }

    pub fn poll_now() -> Self {
        Self::for_wait(Some(0), None)
    }
}

#[derive(Debug)]
pub enum CellCommand {
    ToolResult {
        request_id: String,
        outcome: Result<String, crate::tools::ToolError>,
    },
    Drain(DrainRequest),
    Cancel {
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct ToolCallRequestEvent {
    pub seq: u64,
    pub request_id: String,
    pub tool_name: String,
    pub args_json: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeTerminalKind {
    Completed,
    Failed,
    Cancelled,
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

    pub fn terminal_kind(&self) -> Option<RuntimeTerminalKind> {
        match self {
            Self::Completed { .. } => Some(RuntimeTerminalKind::Completed),
            Self::Failed { .. } => Some(RuntimeTerminalKind::Failed),
            Self::Cancelled { .. } => Some(RuntimeTerminalKind::Cancelled),
            _ => None,
        }
    }

    pub fn is_terminal_summary_event(&self) -> bool {
        self.terminal_kind().is_some()
    }

    pub fn is_visible_to_drain(&self) -> bool {
        !matches!(self, Self::WorkerCompleted(_))
    }
}

pub fn max_event_seq(events: &[RuntimeEvent]) -> u64 {
    events
        .iter()
        .filter_map(RuntimeEvent::seq)
        .max()
        .unwrap_or(0)
}
