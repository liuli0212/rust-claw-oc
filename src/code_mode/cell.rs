use serde_json::Value;

use super::driver::{DriverBoundary, DriverUpdate};
use super::protocol::{max_event_seq, RuntimeEvent, RuntimeTerminalResult};
use super::response::{CellRenderState, ExecLifecycle, ExecProgressKind, ExecRunResult};

const RECENT_EVENT_BUDGET_CHARS: usize = 8_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellWaitState {
    NestedTool { request_id: String },
    Timer { next_due_in_ms: Option<u64> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellPhase {
    Running,
    Waiting(CellWaitState),
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CellTerminalState {
    pub return_value: Option<Value>,
    pub failure: Option<String>,
    pub cancellation: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ActiveCellHandle {
    pub cell_id: String,
    pub phase: CellPhase,
    pub events: Vec<RuntimeEvent>,
    pub terminal_state: Option<CellTerminalState>,
    pub last_publication: Option<ExecRunResult>,
}

impl ActiveCellHandle {
    pub fn new(cell_id: String) -> Self {
        Self {
            cell_id,
            phase: CellPhase::Running,
            events: Vec::new(),
            terminal_state: None,
            last_publication: None,
        }
    }

    pub fn snapshot(&self) -> CellSnapshot {
        let max_seq = max_event_seq(&self.events);
        let recent_events = self.recent_visible_events();

        CellSnapshot {
            cell_id: self.cell_id.clone(),
            phase: self.phase.clone(),
            terminal_state: self.terminal_state.clone(),
            max_seq,
            recent_events,
            nested_tool_calls: self.nested_tool_call_count(),
        }
    }

    pub fn record_driver_update(&mut self, update: &DriverUpdate) {
        self.record_event_batch(&update.batch.events);
        if let DriverBoundary::Terminal(result) = &update.boundary {
            self.record_terminal_result(result);
        }
    }

    pub fn transition_to_failure(&mut self, error: String) {
        self.phase = CellPhase::Failed;
        self.terminal_state = Some(CellTerminalState {
            return_value: None,
            failure: Some(error),
            cancellation: None,
        });
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self.phase,
            CellPhase::Completed | CellPhase::Failed | CellPhase::Cancelled
        )
    }

    /// Count of nested tool calls observed in the event stream.
    pub fn nested_tool_call_count(&self) -> usize {
        self.events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::ToolCallRequested(_)))
            .count()
    }

    pub fn recent_visible_events(&self) -> Vec<RuntimeEvent> {
        let mut budget = RECENT_EVENT_BUDGET_CHARS;
        let mut result = Vec::new();
        for event in self.events.iter().rev() {
            if !event.is_visible_in_snapshot() {
                continue;
            }
            let text = match event {
                RuntimeEvent::Text { text, .. } => text.as_str(),
                RuntimeEvent::Notification { message, .. } => message.as_str(),
                _ => "",
            };
            let chars = text.chars().count();
            if chars > budget {
                break;
            }
            budget -= chars;
            result.push(event.clone());
        }
        result.reverse();
        result
    }

    fn record_event_batch(&mut self, events: &[RuntimeEvent]) {
        for event in events {
            self.update_phase_from_event(event);
            self.events.push(event.clone());
        }
    }

    fn update_phase_from_event(&mut self, event: &RuntimeEvent) {
        match event {
            RuntimeEvent::Text { .. }
            | RuntimeEvent::Notification { .. }
            | RuntimeEvent::Flush { .. } => {
                if matches!(self.phase, CellPhase::Waiting(_)) {
                    self.phase = CellPhase::Running;
                }
            }
            RuntimeEvent::ToolCallRequested(request) => {
                self.phase = CellPhase::Waiting(CellWaitState::NestedTool {
                    request_id: request.request_id.clone(),
                });
            }
            RuntimeEvent::ToolCallResolved { .. } => {
                self.phase = CellPhase::Running;
            }
            RuntimeEvent::WaitingForTimer {
                resume_after_ms, ..
            } => {
                self.phase = CellPhase::Waiting(CellWaitState::Timer {
                    next_due_in_ms: *resume_after_ms,
                });
            }
            RuntimeEvent::TimerRegistrationChanged { .. } | RuntimeEvent::WorkerCompleted(_) => {}
        }
    }

    fn record_terminal_result(&mut self, result: &RuntimeTerminalResult) {
        self.phase = if result.runtime_error.is_some() {
            CellPhase::Failed
        } else if result.cancellation_reason.is_some() {
            CellPhase::Cancelled
        } else {
            CellPhase::Completed
        };
        self.terminal_state = Some(CellTerminalState {
            return_value: result.return_value.clone(),
            failure: result.runtime_error.clone(),
            cancellation: result.cancellation_reason.clone(),
        });
    }
}

#[derive(Debug, Clone)]
pub struct CellSnapshot {
    pub cell_id: String,
    pub phase: CellPhase,
    pub terminal_state: Option<CellTerminalState>,
    pub max_seq: u64,
    pub recent_events: Vec<RuntimeEvent>,
    pub nested_tool_calls: usize,
}

impl CellSnapshot {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.phase,
            CellPhase::Completed | CellPhase::Failed | CellPhase::Cancelled
        )
    }

    pub fn lifecycle(&self) -> ExecLifecycle {
        match self.phase {
            CellPhase::Completed => ExecLifecycle::Completed,
            CellPhase::Failed => ExecLifecycle::Failed,
            CellPhase::Cancelled => ExecLifecycle::Cancelled,
            CellPhase::Running | CellPhase::Waiting(_) => ExecLifecycle::Running,
        }
    }

    pub fn waiting_on_tool_request_id(&self) -> Option<&str> {
        match &self.phase {
            CellPhase::Waiting(CellWaitState::NestedTool { request_id }) => Some(request_id),
            _ => None,
        }
        .map(String::as_str)
    }

    pub fn waiting_on_timer_ms(&self) -> Option<u64> {
        match &self.phase {
            CellPhase::Waiting(CellWaitState::Timer { next_due_in_ms }) => *next_due_in_ms,
            _ => None,
        }
    }

    pub fn build_render_state(&self) -> CellRenderState {
        let mut state = CellRenderState::from_events(&self.recent_events);
        state.lifecycle = self.lifecycle();
        state.waiting_on_tool_request_id = self.waiting_on_tool_request_id().map(ToOwned::to_owned);
        state.waiting_on_timer_ms = self.waiting_on_timer_ms();

        if let Some(terminal) = &self.terminal_state {
            state.return_value = terminal.return_value.clone();
            state.flush_value = None;
            state.failure = terminal.failure.clone();
            state.cancellation = terminal.cancellation.clone();
        }

        state
    }

    pub fn to_exec_result(
        &self,
        progress_kind: Option<ExecProgressKind>,
        flush_value: Option<Value>,
    ) -> ExecRunResult {
        let render = self.build_render_state();

        ExecRunResult {
            cell_id: self.cell_id.clone(),
            output_text: render.output_text,
            return_value: render.return_value,
            flush_value: if progress_kind == Some(ExecProgressKind::ExplicitFlush) {
                flush_value
            } else {
                None
            },
            lifecycle: self.lifecycle(),
            progress_kind: progress_kind.clone(),
            flushed: progress_kind.is_some(),
            waiting_on_tool_request_id: self.waiting_on_tool_request_id().map(str::to_owned),
            waiting_on_timer_ms: self.waiting_on_timer_ms(),
            notifications: render.notifications,
            failure: render.failure,
            cancellation: render.cancellation,
            nested_tool_calls: self.nested_tool_calls,
            truncated: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_active_cell_handle_collects_events_and_renders_snapshot() {
        let mut cell = ActiveCellHandle::new("test-cell".to_string());
        cell.events.push(RuntimeEvent::Text {
            seq: 1,
            text: "hello ".to_string(),
        });
        cell.events.push(RuntimeEvent::Text {
            seq: 2,
            text: "world".to_string(),
        });
        cell.events.push(RuntimeEvent::Notification {
            seq: 3,
            message: "notif".to_string(),
        });
        cell.events.push(RuntimeEvent::Flush {
            seq: 4,
            value: Some(json!({"foo": "bar"})),
        });

        let snapshot = cell.snapshot();
        assert_eq!(snapshot.cell_id, "test-cell");
        assert_eq!(snapshot.max_seq, 4);

        let render = snapshot.build_render_state();
        assert_eq!(render.output_text, "hello \nworld");
        assert_eq!(render.notifications, vec!["notif"]);
        assert_eq!(render.flush_value, Some(json!({"foo": "bar"})));
    }
}
