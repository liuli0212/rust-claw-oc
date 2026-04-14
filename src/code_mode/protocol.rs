use std::collections::HashMap;

use serde_json::Value;

use super::response::ExecRunResult;
use super::response::ExecYieldKind;
use super::runtime;
use super::runtime::value::StoredValue;

pub type RuntimeCellResult = (
    ExecRunResult,
    HashMap<String, StoredValue>,
    runtime::RunCellMetadata,
);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrainRequest {
    pub wait_for_event: bool,
    pub wait_timeout_ms: Option<u64>,
    pub refresh_slice_ms: Option<u64>,
}

impl DrainRequest {
    pub fn to_completion() -> Self {
        Self::default()
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
        request_id: u64,
        outcome: Result<String, crate::tools::ToolError>,
    },
    Drain(DrainRequest),
    Cancel {
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct ToolCallRequest {
    pub seq: u64,
    pub request_id: u64,
    pub tool_name: String,
    pub args_json: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeTerminalKind {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug)]
pub enum RuntimeEvent {
    Text {
        seq: u64,
        chunk: String,
    },
    Notification {
        seq: u64,
        message: String,
    },
    Yield {
        seq: u64,
        kind: ExecYieldKind,
        value: Option<Value>,
        resume_after_ms: Option<u64>,
    },
    ToolCallRequested(ToolCallRequest),
    ToolCallResolved {
        seq: u64,
        request_id: u64,
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
    WorkerCompleted(Result<RuntimeCellResult, crate::tools::ToolError>),
}

impl Clone for RuntimeEvent {
    fn clone(&self) -> Self {
        match self {
            Self::Text { seq, chunk } => Self::Text {
                seq: *seq,
                chunk: chunk.clone(),
            },
            Self::Notification { seq, message } => Self::Notification {
                seq: *seq,
                message: message.clone(),
            },
            Self::Yield {
                seq,
                kind,
                value,
                resume_after_ms,
            } => Self::Yield {
                seq: *seq,
                kind: kind.clone(),
                value: value.clone(),
                resume_after_ms: *resume_after_ms,
            },
            Self::ToolCallRequested(request) => Self::ToolCallRequested(request.clone()),
            Self::ToolCallResolved {
                seq,
                request_id,
                ok,
            } => Self::ToolCallResolved {
                seq: *seq,
                request_id: *request_id,
                ok: *ok,
            },
            Self::Completed { seq, return_value } => Self::Completed {
                seq: *seq,
                return_value: return_value.clone(),
            },
            Self::Failed { seq, error } => Self::Failed {
                seq: *seq,
                error: error.clone(),
            },
            Self::Cancelled { seq, reason } => Self::Cancelled {
                seq: *seq,
                reason: reason.clone(),
            },
            Self::WorkerCompleted(result) => Self::WorkerCompleted(match result {
                Ok((summary, stored_values, metadata)) => {
                    Ok((summary.clone(), stored_values.clone(), metadata.clone()))
                }
                Err(err) => Err(crate::tools::ToolError::ExecutionFailed(err.to_string())),
            }),
        }
    }
}

impl RuntimeEvent {
    pub fn seq(&self) -> Option<u64> {
        match self {
            Self::Text { seq, .. }
            | Self::Notification { seq, .. }
            | Self::Yield { seq, .. }
            | Self::ToolCallResolved { seq, .. }
            | Self::Completed { seq, .. }
            | Self::Failed { seq, .. }
            | Self::Cancelled { seq, .. } => Some(*seq),
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
        !self.is_worker_completion()
    }

    pub fn is_worker_completion(&self) -> bool {
        matches!(self, Self::WorkerCompleted(_))
    }

    pub fn apply_seq_offset(&mut self, offset: u64) {
        if offset == 0 {
            return;
        }

        match self {
            Self::Text { seq, .. }
            | Self::Notification { seq, .. }
            | Self::Yield { seq, .. }
            | Self::ToolCallResolved { seq, .. }
            | Self::Completed { seq, .. }
            | Self::Failed { seq, .. }
            | Self::Cancelled { seq, .. } => *seq += offset,
            Self::ToolCallRequested(request) => {
                request.seq += offset;
            }
            Self::WorkerCompleted(_) => {}
        }
    }
}

pub fn max_event_seq(events: &[RuntimeEvent]) -> u64 {
    events
        .iter()
        .filter_map(RuntimeEvent::seq)
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_drain_request_defaults_to_non_waiting_scaffold() {
        let request = DrainRequest::default();

        assert!(!request.wait_for_event);
        assert_eq!(request.wait_timeout_ms, None);
        assert_eq!(request.refresh_slice_ms, None);
    }

    #[test]
    fn test_drain_request_presets_express_current_exec_and_wait_intents() {
        assert_eq!(DrainRequest::to_completion(), DrainRequest::default());
        assert_eq!(
            DrainRequest::wait_for_next_event(),
            DrainRequest {
                wait_for_event: true,
                wait_timeout_ms: None,
                refresh_slice_ms: None,
            }
        );
        assert_eq!(
            DrainRequest::for_wait(Some(25), Some(50)),
            DrainRequest {
                wait_for_event: true,
                wait_timeout_ms: Some(25),
                refresh_slice_ms: Some(50),
            }
        );
        assert_eq!(
            DrainRequest::poll_now(),
            DrainRequest {
                wait_for_event: true,
                wait_timeout_ms: Some(0),
                refresh_slice_ms: None,
            }
        );
    }

    #[test]
    fn test_runtime_event_terminal_helpers_distinguish_terminal_and_worker_events() {
        let completed = RuntimeEvent::Completed {
            seq: 3,
            return_value: None,
        };
        let worker_completed = RuntimeEvent::WorkerCompleted(Ok((
            ExecRunResult {
                cell_id: "cell_1".to_string(),
                output_text: String::new(),
                return_value: None,
                yield_value: None,
                yielded: false,
                yield_kind: None,
                notifications: Vec::new(),
                nested_tool_calls: 0,
                truncated: false,
            },
            HashMap::new(),
            runtime::RunCellMetadata::default(),
        )));

        assert_eq!(
            completed.terminal_kind(),
            Some(RuntimeTerminalKind::Completed)
        );
        assert!(completed.is_terminal_summary_event());
        assert!(completed.is_visible_to_drain());
        assert!(!completed.is_worker_completion());

        assert_eq!(worker_completed.terminal_kind(), None);
        assert!(!worker_completed.is_terminal_summary_event());
        assert!(!worker_completed.is_visible_to_drain());
        assert!(worker_completed.is_worker_completion());
    }

    #[test]
    fn test_runtime_event_seq_ignores_worker_completion() {
        let event = RuntimeEvent::WorkerCompleted(Err(crate::tools::ToolError::ExecutionFailed(
            "boom".to_string(),
        )));

        assert_eq!(event.seq(), None);
        assert_eq!(max_event_seq(&[event]), 0);
    }
}
