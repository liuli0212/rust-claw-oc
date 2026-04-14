use std::collections::HashSet;

use super::protocol::{max_event_seq, RuntimeEvent, RuntimeTerminalKind};
use super::response::{
    timer_pending_resume_after_ms, DrainRenderState, ExecRunResult, ExecYieldKind,
};

const RECENT_EVENT_BUDGET_CHARS: usize = 8_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellStatus {
    Starting,
    Running,
    WaitingOnTool { request_id: u64 },
    WaitingOnJsTimer { next_due_in_ms: Option<u64> },
    Completed,
    Failed,
    Cancelled,
}


#[derive(Debug, Clone)]
pub struct ActiveCellHandle {
    pub cell_id: String,
    pub code: String,
    pub visible_tools: Vec<String>,
    pub status: CellStatus,
    pub last_event_seq: u64,
    pub recent_events: Vec<RuntimeEvent>,
    pub recent_events_truncated: bool,
    pub total_nested_tool_calls: usize,
}

impl ActiveCellHandle {
    pub fn from_initial_yield(
        cell_id: String,
        code: String,
        visible_tools: Vec<String>,
        summary: &ExecRunResult,
        events: Vec<RuntimeEvent>,
        last_event_seq: u64,
    ) -> Self {
        let status = status_from_events(&events, summary);
        let (recent_events, recent_events_truncated) = bounded_recent_events(events);
        Self {
            cell_id,
            code,
            visible_tools,
            status,
            last_event_seq,
            recent_events,
            recent_events_truncated,
            total_nested_tool_calls: summary.nested_tool_calls,
        }
    }

    pub fn advance_with_yield(
        self,
        current_turn_nested_tool_calls: usize,
        summary: &ExecRunResult,
        events: Vec<RuntimeEvent>,
        last_event_seq: u64,
    ) -> Self {
        let status = status_from_events(&events, summary);
        let (recent_events, recent_events_truncated) = bounded_recent_events(events);
        Self {
            cell_id: self.cell_id,
            code: self.code,
            visible_tools: self.visible_tools,
            status,
            last_event_seq,
            recent_events,
            recent_events_truncated,
            total_nested_tool_calls: self.total_nested_tool_calls + current_turn_nested_tool_calls,
        }
    }

    pub fn advance_with_events(
        self,
        events: Vec<RuntimeEvent>,
        last_event_seq: u64,
    ) -> Self {
        let mut combined_events = self.recent_events;
        combined_events.extend(events);
        let status = status_from_event_slice(&combined_events).unwrap_or(self.status.clone());
        let (recent_events, recent_events_truncated) = bounded_recent_events(combined_events);
        Self {
            cell_id: self.cell_id,
            code: self.code,
            visible_tools: self.visible_tools,
            status,
            last_event_seq,
            recent_events,
            recent_events_truncated: self.recent_events_truncated || recent_events_truncated,
            total_nested_tool_calls: self.total_nested_tool_calls,
        }
    }

        pub fn rebase_runtime_events(&self, events: &mut [RuntimeEvent]) -> u64 {
        for event in events.iter_mut() {
            event.apply_seq_offset(self.last_event_seq);
        }
        max_event_seq(events).max(self.last_event_seq)
    }

    pub fn drain_render_state(&self) -> DrainRenderState {
        DrainRenderState::from_events(&self.recent_events)
    }

    pub fn drain_snapshot(&self) -> CellDrainSnapshot {
        CellDrainSnapshot {
            status: self.status.clone(),
            nested_tool_calls: self.total_nested_tool_calls,
            render_state: self.drain_render_state(),
            truncated: self.recent_events_truncated,
        }
    }

    pub fn render_recent_events(&self, truncated: bool) -> String {
        let mut snapshot = self.drain_snapshot();
        snapshot.truncated |= truncated;
        snapshot.render(&self.cell_id)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CellDrainSnapshot {
    pub status: CellStatus,
    pub nested_tool_calls: usize,
    pub render_state: DrainRenderState,
    pub truncated: bool,
}

impl CellDrainSnapshot {
    pub fn render(&self, cell_id: &str) -> String {
        self.render_state.render_output_with_status(
            cell_id,
            self.nested_tool_calls,
            self.truncated,
            Some(&self.status),
        )
    }

    pub fn to_exec_result(&self, cell_id: impl Into<String>) -> ExecRunResult {
        let yielded = matches!(
            &self.status,
            CellStatus::Starting
                | CellStatus::Running
                | CellStatus::WaitingOnTool { .. }
                | CellStatus::WaitingOnJsTimer { .. }
        );

        ExecRunResult {
            cell_id: cell_id.into(),
            output_text: self.render_state.output_text.clone(),
            return_value: if yielded {
                None
            } else {
                self.render_state.return_value.clone()
            },
            yield_value: if yielded {
                self.render_state.yield_value.clone()
            } else {
                None
            },
            yielded,
            yield_kind: if yielded {
                self.render_state.yield_kind.clone()
            } else {
                None
            },
            notifications: self.render_state.notifications.clone(),
            nested_tool_calls: self.nested_tool_calls,
            truncated: self.truncated,
        }
    }
}

fn bounded_recent_events(events: Vec<RuntimeEvent>) -> (Vec<RuntimeEvent>, bool) {
    let mut retained = Vec::new();
    let mut budget_used = 0usize;
    let mut truncated = false;

    for event in events.into_iter().rev() {
        let event_cost = recent_event_cost(&event);
        if !retained.is_empty() && budget_used + event_cost > RECENT_EVENT_BUDGET_CHARS {
            truncated = true;
            break;
        }
        budget_used += event_cost;
        retained.push(event);
    }

    retained.reverse();
    (retained, truncated)
}

fn recent_event_cost(event: &RuntimeEvent) -> usize {
    match event {
        RuntimeEvent::Text { chunk, .. } => chunk.len(),
        RuntimeEvent::Notification { message, .. } => message.len() + 32,
        RuntimeEvent::Yield { value, .. } => {
            64 + value
                .as_ref()
                .map(|item| item.to_string().len())
                .unwrap_or(0)
        }
        RuntimeEvent::ToolCallRequested(request) => {
            request.tool_name.len() + request.args_json.len() + 64
        }
        RuntimeEvent::ToolCallResolved { .. } => 32,
        RuntimeEvent::Completed { return_value, .. } => {
            64 + return_value
                .as_ref()
                .map(|item| item.to_string().len())
                .unwrap_or(0)
        }
        RuntimeEvent::Failed { error, .. } => error.len() + 64,
        RuntimeEvent::Cancelled { reason, .. } => reason.len() + 64,
        RuntimeEvent::WorkerCompleted(_) => 0,
    }
}

fn status_from_event_slice(events: &[RuntimeEvent]) -> Option<CellStatus> {
    let mut resolved_requests = HashSet::new();

    for event in events.iter().rev() {
        if let Some(kind) = event.terminal_kind() {
            return Some(match kind {
                RuntimeTerminalKind::Completed => CellStatus::Completed,
                RuntimeTerminalKind::Failed => CellStatus::Failed,
                RuntimeTerminalKind::Cancelled => CellStatus::Cancelled,
            });
        }

        match event {
            RuntimeEvent::Yield {
                kind: ExecYieldKind::Timer,
                resume_after_ms,
                ..
            } => {
                return Some(CellStatus::WaitingOnJsTimer {
                    next_due_in_ms: *resume_after_ms,
                });
            }
            RuntimeEvent::Yield {
                kind: ExecYieldKind::Manual,
                ..
            } => return Some(CellStatus::Running),
            RuntimeEvent::Completed { .. }
            | RuntimeEvent::Failed { .. }
            | RuntimeEvent::Cancelled { .. } => {
                unreachable!("terminal events are handled before status matching")
            }
            RuntimeEvent::ToolCallResolved { request_id, .. } => {
                resolved_requests.insert(*request_id);
            }
            RuntimeEvent::ToolCallRequested(request)
                if !resolved_requests.contains(&request.request_id) =>
            {
                return Some(CellStatus::WaitingOnTool {
                    request_id: request.request_id,
                });
            }
            RuntimeEvent::ToolCallRequested(_) => {}
            RuntimeEvent::Text { .. } | RuntimeEvent::Notification { .. } => {}
            RuntimeEvent::WorkerCompleted(_) => {}
        }
    }

    None
}

fn status_from_events(events: &[RuntimeEvent], summary: &ExecRunResult) -> CellStatus {
    status_from_event_slice(events).unwrap_or_else(|| status_from_summary(summary))
}

fn status_from_summary(summary: &ExecRunResult) -> CellStatus {
    if !summary.yielded {
        return CellStatus::Completed;
    }

    match summary.yield_kind.as_ref() {
        Some(ExecYieldKind::Timer) => CellStatus::WaitingOnJsTimer {
            next_due_in_ms: timer_pending_resume_after_ms(summary.yield_value.as_ref()),
        },
        Some(ExecYieldKind::Manual) | None => CellStatus::Running,
    }
}

