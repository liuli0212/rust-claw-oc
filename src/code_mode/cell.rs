use super::protocol::{max_event_seq, RuntimeEvent};
use super::response::{DrainRenderState, ExecLifecycle, ExecRunResult};

const RECENT_EVENT_BUDGET_CHARS: usize = 8_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellStatus {
    Starting,
    Running,
    WaitingOnTool { request_id: String },
    WaitingOnJsTimer { next_due_in_ms: Option<u64> },
    Flushed,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct ActiveCellHandle {
    pub cell_id: String,
    pub status: CellStatus,
    pub events: Vec<RuntimeEvent>,
    pub last_publication: Option<ExecRunResult>,
    pub last_summary: Option<ExecRunResult>,
}

impl ActiveCellHandle {
    pub fn new(cell_id: String) -> Self {
        Self {
            cell_id,
            status: CellStatus::Starting,
            events: Vec::new(),
            last_publication: None,
            last_summary: None,
        }
    }

    pub fn drain_snapshot(&self) -> CellDrainSnapshot {
        let max_seq = max_event_seq(&self.events);
        let recent_events = self.recent_visible_events();

        CellDrainSnapshot {
            cell_id: self.cell_id.clone(),
            status: self.status.clone(),
            last_summary: self.last_summary.clone(),
            max_seq,
            recent_events,
        }
    }

    pub fn apply_drain_batch(&mut self, batch: &crate::code_mode::driver::DriverDrainBatch) {
        for event in &batch.events {
            // Update status from events
            match event {
                RuntimeEvent::ToolCallRequested(req) => {
                    self.status = CellStatus::WaitingOnTool {
                        request_id: req.request_id.clone(),
                    };
                }
                RuntimeEvent::ToolCallResolved { .. } => {
                    self.status = CellStatus::Running;
                }
                RuntimeEvent::WaitingForTimer {
                    resume_after_ms, ..
                } => {
                    self.status = CellStatus::WaitingOnJsTimer {
                        next_due_in_ms: *resume_after_ms,
                    };
                }
                RuntimeEvent::Flush { .. } => {
                    self.status = CellStatus::Flushed;
                }
                RuntimeEvent::Completed { .. } => {
                    self.status = CellStatus::Completed;
                }
                RuntimeEvent::Failed { .. } => {
                    self.status = CellStatus::Failed;
                }
                RuntimeEvent::Cancelled { .. } => {
                    self.status = CellStatus::Cancelled;
                }
                _ => {}
            }
            self.events.push(event.clone());
        }
        if let Some(terminal) = &batch.terminal_result {
            self.last_summary = Some(terminal.0.clone());
            if let Some(summary) = self.last_summary.as_ref() {
                self.status = match &summary.lifecycle {
                    ExecLifecycle::Running => self.status.clone(),
                    ExecLifecycle::Completed => CellStatus::Completed,
                    ExecLifecycle::Failed => CellStatus::Failed,
                    ExecLifecycle::Cancelled => CellStatus::Cancelled,
                };
            }
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            CellStatus::Completed | CellStatus::Failed | CellStatus::Cancelled
        )
    }

    /// Count of nested tool calls observed in the event stream.
    pub fn nested_tool_call_count(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e, RuntimeEvent::ToolCallRequested(_)))
            .count()
    }

    pub fn recent_visible_events(&self) -> Vec<RuntimeEvent> {
        let mut budget = RECENT_EVENT_BUDGET_CHARS;
        let mut result = Vec::new();
        for event in self.events.iter().rev() {
            if !event.is_visible_to_drain() {
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
}

#[derive(Debug, Clone)]
pub struct CellDrainSnapshot {
    pub cell_id: String,
    pub status: CellStatus,
    pub last_summary: Option<ExecRunResult>,
    pub max_seq: u64,
    pub recent_events: Vec<RuntimeEvent>,
}

impl CellDrainSnapshot {
    pub fn render_state(&self) -> DrainRenderState {
        let mut state = DrainRenderState::from_events(&self.recent_events);

        // If we have a terminal last_summary, it is the final authority on the execution state.
        if let Some(summary) = &self.last_summary {
            state.return_value = summary.return_value.clone();
            state.flush_value = summary.flush_value.clone();
            state.lifecycle = summary.lifecycle.clone();
            state.progress_kind = summary.progress_kind.clone();
            state.failure = summary.failure.clone();
            state.cancellation = summary.cancellation.clone();
        }

        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_mode::response::ExecProgressKind;
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
        cell.last_summary = Some(ExecRunResult {
            cell_id: "test-cell".to_string(),
            output_text: String::new(),
            return_value: None,
            flush_value: Some(json!({"foo": "bar"})),
            lifecycle: ExecLifecycle::Running,
            progress_kind: Some(ExecProgressKind::ExplicitFlush),
            flushed: true,
            waiting_on_tool_request_id: None,
            waiting_on_timer_ms: None,
            notifications: Vec::new(),
            failure: None,
            cancellation: None,
            nested_tool_calls: 0,
            truncated: false,
        });

        let snapshot = cell.drain_snapshot();
        assert_eq!(snapshot.cell_id, "test-cell");
        assert_eq!(snapshot.max_seq, 4);

        let render = snapshot.render_state();
        assert_eq!(render.output_text, "hello \nworld");
        assert_eq!(render.notifications, vec!["notif"]);
        assert_eq!(render.flush_value, Some(json!({"foo": "bar"})));
    }
}
