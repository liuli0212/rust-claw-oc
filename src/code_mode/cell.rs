use super::protocol::{max_event_seq, RuntimeEvent};
use super::response::{DrainRenderState, ExecRunResult, ExecYieldKind};

const RECENT_EVENT_BUDGET_CHARS: usize = 8_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellStatus {
    Starting,
    Running,
    WaitingOnTool { request_id: String },
    WaitingOnJsTimer { next_due_in_ms: Option<u64> },
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct ActiveCellHandle {
    pub cell_id: String,
    pub status: CellStatus,
    pub events: Vec<RuntimeEvent>,
    pub last_summary: Option<ExecRunResult>,
}

impl ActiveCellHandle {
    pub fn new(cell_id: String) -> Self {
        Self {
            cell_id,
            status: CellStatus::Starting,
            events: Vec::new(),
            last_summary: None,
        }
    }

    pub fn finish_turn_with_yield(&mut self, summary: ExecRunResult) {
        self.last_summary = Some(summary);
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
                RuntimeEvent::Yield { kind: ExecYieldKind::Timer, resume_after_ms, .. } => {
                    self.status = CellStatus::WaitingOnJsTimer {
                        next_due_in_ms: *resume_after_ms,
                    };
                }
                RuntimeEvent::Yield { .. } => {
                    self.status = CellStatus::Running;
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
            // Infer terminal status from the ExecRunResult if not already set
            if self.last_summary.as_ref().is_some_and(|s| s.yielded) {
                // yielded — keep running status
            } else {
                self.status = CellStatus::Completed;
            }
        }
    }

    /// Whether the cell is in a terminal state (completed, failed, or cancelled).
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
        let mut text_buf = String::new();
        let mut notifications = Vec::new();

        for event in &self.recent_events {
            match event {
                RuntimeEvent::Text { text, .. } => {
                    if !text_buf.is_empty() && !text.is_empty() {
                        text_buf.push('\n');
                    }
                    text_buf.push_str(text);
                }
                RuntimeEvent::Notification { message, .. } => {
                    notifications.push(message.clone());
                }
                _ => {}
            }
        }

        DrainRenderState {
            output_text: text_buf,
            notifications,
            return_value: self.last_summary.as_ref().and_then(|s| s.return_value.clone()),
            yield_value: self.last_summary.as_ref().and_then(|s| s.yield_value.clone()),
            yield_kind: self.last_summary.as_ref().and_then(|s| s.yield_kind.clone()),
            failure: None,
            cancellation: None,
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
        cell.events.push(RuntimeEvent::Text { seq: 1, text: "hello ".to_string() });
        cell.events.push(RuntimeEvent::Text { seq: 2, text: "world".to_string() });
        cell.events.push(RuntimeEvent::Notification { seq: 3, message: "notif".to_string() });
        cell.events.push(RuntimeEvent::Yield {
            seq: 4,
            kind: ExecYieldKind::Manual,
            value: Some(json!({"foo": "bar"})),
            resume_after_ms: None,
        });
        cell.last_summary = Some(ExecRunResult {
            cell_id: "test-cell".to_string(),
            output_text: String::new(),
            return_value: None,
            yield_value: Some(json!({"foo": "bar"})),
            yielded: true,
            yield_kind: Some(ExecYieldKind::Manual),
            notifications: Vec::new(),
            nested_tool_calls: 0,
            truncated: false,
        });

        let snapshot = cell.drain_snapshot();
        assert_eq!(snapshot.cell_id, "test-cell");
        assert_eq!(snapshot.max_seq, 4);
        
        let render = snapshot.render_state();
        assert_eq!(render.output_text, "hello \nworld");
        assert_eq!(render.notifications, vec!["notif"]);
        assert_eq!(render.yield_value, Some(json!({"foo": "bar"})));
    }
}
