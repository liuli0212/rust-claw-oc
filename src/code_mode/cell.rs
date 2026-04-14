use std::collections::HashSet;

use super::protocol::{max_event_seq, RuntimeEvent, RuntimeTerminalKind};
use super::response::{
    timer_pending_resume_after_ms, DrainRenderState, ExecRunResult, ExecYieldKind,
};
use super::runtime;
use super::runtime::callbacks::RecordedToolCall;
use super::runtime::timers::RecordedTimerCall;
use super::runtime::RunCellMetadata;

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

#[derive(Debug, Clone, Default)]
pub struct CellResumeState {
    pub replayed_tool_calls: Vec<RecordedToolCall>,
    pub recorded_timer_calls: Vec<RecordedTimerCall>,
    pub suppressed_text_calls: usize,
    pub suppressed_notification_calls: usize,
    pub skipped_yields: usize,
    pub total_nested_tool_calls: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CellResumeProgressDelta {
    pub replayed_tool_calls_delta: Vec<RecordedToolCall>,
    pub recorded_timer_calls: Option<Vec<RecordedTimerCall>>,
    pub suppressed_text_calls_delta: usize,
    pub suppressed_notification_calls_delta: usize,
    pub skipped_yields_delta: usize,
    pub total_nested_tool_calls_delta: usize,
}

impl CellResumeProgressDelta {
    pub fn merge(&mut self, mut other: Self) {
        self.replayed_tool_calls_delta
            .append(&mut other.replayed_tool_calls_delta);
        if let Some(recorded_timer_calls) = other.recorded_timer_calls.take() {
            self.recorded_timer_calls = Some(recorded_timer_calls);
        }
        self.suppressed_text_calls_delta += other.suppressed_text_calls_delta;
        self.suppressed_notification_calls_delta += other.suppressed_notification_calls_delta;
        self.skipped_yields_delta += other.skipped_yields_delta;
        self.total_nested_tool_calls_delta += other.total_nested_tool_calls_delta;
    }
}

impl CellResumeState {
    pub fn from_initial_yield(summary: &ExecRunResult, metadata: &RunCellMetadata) -> Self {
        Self {
            replayed_tool_calls: metadata.newly_recorded_tool_calls.clone(),
            recorded_timer_calls: metadata.timer_calls.clone(),
            suppressed_text_calls: metadata.total_text_calls,
            suppressed_notification_calls: metadata.total_notification_calls,
            skipped_yields: if matches!(summary.yield_kind.as_ref(), Some(ExecYieldKind::Manual)) {
                1
            } else {
                0
            },
            total_nested_tool_calls: summary.nested_tool_calls,
        }
    }

    pub fn advance_with_yield(
        mut self,
        current_turn_nested_tool_calls: usize,
        summary: &ExecRunResult,
        metadata: &RunCellMetadata,
    ) -> Self {
        self.replayed_tool_calls
            .extend(metadata.newly_recorded_tool_calls.clone());
        self.recorded_timer_calls = metadata.timer_calls.clone();
        self.suppressed_text_calls = metadata.total_text_calls;
        self.suppressed_notification_calls = metadata.total_notification_calls;
        if matches!(summary.yield_kind.as_ref(), Some(ExecYieldKind::Manual)) {
            self.skipped_yields += 1;
        }
        self.total_nested_tool_calls += current_turn_nested_tool_calls;
        self
    }

    pub fn advance_with_progress(mut self, progress: &CellResumeProgressDelta) -> Self {
        self.replayed_tool_calls
            .extend(progress.replayed_tool_calls_delta.clone());
        if let Some(recorded_timer_calls) = &progress.recorded_timer_calls {
            self.recorded_timer_calls = recorded_timer_calls.clone();
        }
        self.suppressed_text_calls += progress.suppressed_text_calls_delta;
        self.suppressed_notification_calls += progress.suppressed_notification_calls_delta;
        self.skipped_yields += progress.skipped_yields_delta;
        self.total_nested_tool_calls += progress.total_nested_tool_calls_delta;
        self
    }

    pub fn total_nested_tool_calls(&self, current_turn_nested_tool_calls: usize) -> usize {
        self.total_nested_tool_calls + current_turn_nested_tool_calls
    }

    pub fn to_runtime_resume_state(&self) -> runtime::ResumeState {
        runtime::ResumeState {
            replayed_tool_calls: self.replayed_tool_calls.clone(),
            recorded_timer_calls: self.recorded_timer_calls.clone(),
            skipped_yields: self.skipped_yields,
            suppressed_text_calls: self.suppressed_text_calls,
            suppressed_notification_calls: self.suppressed_notification_calls,
        }
    }
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
    pub resume_state: CellResumeState,
    pub pending_resume_progress: CellResumeProgressDelta,
}

impl ActiveCellHandle {
    pub fn from_initial_yield(
        cell_id: String,
        code: String,
        visible_tools: Vec<String>,
        summary: &ExecRunResult,
        metadata: &RunCellMetadata,
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
            resume_state: CellResumeState::from_initial_yield(summary, metadata),
            pending_resume_progress: CellResumeProgressDelta::default(),
        }
    }

    pub fn advance_with_yield(
        self,
        current_turn_nested_tool_calls: usize,
        summary: &ExecRunResult,
        metadata: &RunCellMetadata,
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
            resume_state: self.resume_state.advance_with_yield(
                current_turn_nested_tool_calls,
                summary,
                metadata,
            ),
            pending_resume_progress: CellResumeProgressDelta::default(),
        }
    }

    pub fn advance_with_events(
        self,
        events: Vec<RuntimeEvent>,
        progress: CellResumeProgressDelta,
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
            resume_state: self.resume_state,
            pending_resume_progress: {
                let mut pending_resume_progress = self.pending_resume_progress;
                pending_resume_progress.merge(progress);
                pending_resume_progress
            },
        }
    }

    pub fn runtime_resume_state(&self) -> runtime::ResumeState {
        self.resume_state
            .clone()
            .advance_with_progress(&self.pending_resume_progress)
            .to_runtime_resume_state()
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
            nested_tool_calls: self.resume_state.total_nested_tool_calls(
                self.pending_resume_progress.total_nested_tool_calls_delta,
            ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_mode::protocol::ToolCallRequest;

    #[test]
    fn test_status_from_events_prefers_unresolved_tool_requests() {
        let summary = ExecRunResult {
            cell_id: "cell_1".to_string(),
            output_text: String::new(),
            return_value: None,
            yield_value: Some(serde_json::json!("pause")),
            yielded: true,
            yield_kind: Some(ExecYieldKind::Manual),
            notifications: Vec::new(),
            nested_tool_calls: 0,
            truncated: false,
        };
        let events = vec![RuntimeEvent::ToolCallRequested(ToolCallRequest {
            seq: 1,
            request_id: 7,
            tool_name: "echo_tool".to_string(),
            args_json: "{}".to_string(),
        })];

        assert_eq!(
            status_from_events(&events, &summary),
            CellStatus::WaitingOnTool { request_id: 7 }
        );
    }

    #[test]
    fn test_active_cell_rebases_runtime_event_sequences() {
        let active_cell = ActiveCellHandle {
            cell_id: "cell_1".to_string(),
            code: "yield_control()".to_string(),
            visible_tools: Vec::new(),
            status: CellStatus::Running,
            last_event_seq: 3,
            recent_events: Vec::new(),
            recent_events_truncated: false,
            resume_state: CellResumeState::default(),
            pending_resume_progress: CellResumeProgressDelta::default(),
        };
        let mut events = vec![
            RuntimeEvent::Text {
                seq: 1,
                chunk: "hello".to_string(),
            },
            RuntimeEvent::Yield {
                seq: 2,
                kind: ExecYieldKind::Manual,
                value: Some(serde_json::json!("pause")),
                resume_after_ms: None,
            },
        ];

        let last_event_seq = active_cell.rebase_runtime_events(&mut events);

        assert_eq!(last_event_seq, 5);
        assert_eq!(events[0].seq(), Some(4));
        assert_eq!(events[1].seq(), Some(5));
    }

    #[test]
    fn test_active_cell_renders_recent_events() {
        let active_cell = ActiveCellHandle {
            cell_id: "cell_2".to_string(),
            code: "text(\"hello\")".to_string(),
            visible_tools: Vec::new(),
            status: CellStatus::Running,
            last_event_seq: 2,
            recent_events: vec![
                RuntimeEvent::Text {
                    seq: 1,
                    chunk: "hello".to_string(),
                },
                RuntimeEvent::Yield {
                    seq: 2,
                    kind: ExecYieldKind::Manual,
                    value: Some(serde_json::json!("pause")),
                    resume_after_ms: None,
                },
            ],
            recent_events_truncated: false,
            resume_state: CellResumeState {
                total_nested_tool_calls: 2,
                ..CellResumeState::default()
            },
            pending_resume_progress: CellResumeProgressDelta::default(),
        };

        let state = active_cell.drain_render_state();
        let snapshot = active_cell.drain_snapshot();
        let rendered = active_cell.render_recent_events(false);

        assert_eq!(state.output_text, "hello");
        assert_eq!(state.yield_kind, Some(ExecYieldKind::Manual));
        assert_eq!(snapshot.status, CellStatus::Running);
        assert_eq!(snapshot.nested_tool_calls, 2);
        assert_eq!(snapshot.render_state, state);
        assert!(!snapshot.truncated);
        assert_eq!(snapshot.render("cell_2"), rendered);
        assert!(rendered.contains("yielded after 2 nested tool call(s)"));
        assert!(rendered.contains("Text output:"));
        assert!(rendered.contains("Yield value:"));
    }

    #[test]
    fn test_active_cell_advance_with_events_updates_status_and_appends_recent_events() {
        let active_cell = ActiveCellHandle {
            cell_id: "cell_live_1".to_string(),
            code: "tool_call()".to_string(),
            visible_tools: vec!["read_file".to_string()],
            status: CellStatus::Running,
            last_event_seq: 1,
            recent_events: vec![RuntimeEvent::Text {
                seq: 1,
                chunk: "hello".to_string(),
            }],
            recent_events_truncated: false,
            resume_state: CellResumeState::default(),
            pending_resume_progress: CellResumeProgressDelta::default(),
        };

        let advanced = active_cell.advance_with_events(
            vec![RuntimeEvent::ToolCallRequested(ToolCallRequest {
                seq: 2,
                request_id: 7,
                tool_name: "read_file".to_string(),
                args_json: "{}".to_string(),
            })],
            CellResumeProgressDelta::default(),
            2,
        );

        assert_eq!(advanced.status, CellStatus::WaitingOnTool { request_id: 7 });
        assert_eq!(advanced.last_event_seq, 2);
        assert_eq!(advanced.recent_events.len(), 2);
        assert!(!advanced.recent_events_truncated);
        assert!(matches!(
            advanced.recent_events.last(),
            Some(RuntimeEvent::ToolCallRequested(ToolCallRequest {
                request_id: 7,
                ..
            }))
        ));
    }

    #[test]
    fn test_active_cell_advance_with_events_preserves_truncation_and_tail_status() {
        let active_cell = ActiveCellHandle {
            cell_id: "cell_live_2".to_string(),
            code: "yield_control()".to_string(),
            visible_tools: Vec::new(),
            status: CellStatus::Running,
            last_event_seq: 1,
            recent_events: vec![RuntimeEvent::Text {
                seq: 1,
                chunk: "x".repeat(RECENT_EVENT_BUDGET_CHARS + 1),
            }],
            recent_events_truncated: false,
            resume_state: CellResumeState::default(),
            pending_resume_progress: CellResumeProgressDelta::default(),
        };

        let advanced = active_cell.advance_with_events(
            vec![RuntimeEvent::Yield {
                seq: 2,
                kind: ExecYieldKind::Manual,
                value: Some(serde_json::json!("pause")),
                resume_after_ms: None,
            }],
            CellResumeProgressDelta::default(),
            2,
        );

        assert_eq!(advanced.status, CellStatus::Running);
        assert_eq!(advanced.last_event_seq, 2);
        assert!(advanced.recent_events_truncated);
        assert_eq!(advanced.recent_events.len(), 1);
        assert!(matches!(
            advanced.recent_events.first(),
            Some(RuntimeEvent::Yield {
                kind: ExecYieldKind::Manual,
                ..
            })
        ));
    }

    #[test]
    fn test_active_cell_advance_with_events_applies_resume_progress_delta() {
        let active_cell = ActiveCellHandle {
            cell_id: "cell_live_3".to_string(),
            code: "tool_call()".to_string(),
            visible_tools: vec!["read_file".to_string()],
            status: CellStatus::Running,
            last_event_seq: 1,
            recent_events: Vec::new(),
            recent_events_truncated: false,
            resume_state: CellResumeState {
                suppressed_text_calls: 1,
                suppressed_notification_calls: 2,
                skipped_yields: 3,
                total_nested_tool_calls: 4,
                ..CellResumeState::default()
            },
            pending_resume_progress: CellResumeProgressDelta::default(),
        };

        let advanced = active_cell.advance_with_events(
            Vec::new(),
            CellResumeProgressDelta {
                replayed_tool_calls_delta: vec![RecordedToolCall {
                    tool_name: "read_file".to_string(),
                    args_json: "{}".to_string(),
                    result_json: "{\"ok\":true}".to_string(),
                }],
                recorded_timer_calls: Some(vec![RecordedTimerCall {
                    timer_id: "timer_1".to_string(),
                    delay_ms: 25,
                    due_at_unix_ms: 1_000,
                    completed: false,
                    cleared: false,
                }]),
                suppressed_text_calls_delta: 2,
                suppressed_notification_calls_delta: 1,
                skipped_yields_delta: 1,
                total_nested_tool_calls_delta: 3,
            },
            1,
        );

        assert_eq!(advanced.status, CellStatus::Running);
        assert_eq!(advanced.last_event_seq, 1);
        assert!(advanced.resume_state.replayed_tool_calls.is_empty());
        assert!(advanced.resume_state.recorded_timer_calls.is_empty());
        assert_eq!(advanced.resume_state.suppressed_text_calls, 1);
        assert_eq!(advanced.resume_state.suppressed_notification_calls, 2);
        assert_eq!(advanced.resume_state.skipped_yields, 3);
        assert_eq!(advanced.resume_state.total_nested_tool_calls, 4);
        assert_eq!(
            advanced
                .pending_resume_progress
                .replayed_tool_calls_delta
                .len(),
            1
        );
        assert_eq!(
            advanced
                .pending_resume_progress
                .recorded_timer_calls
                .as_ref()
                .map(Vec::len),
            Some(1)
        );
        assert_eq!(
            advanced.pending_resume_progress.suppressed_text_calls_delta,
            2
        );
        assert_eq!(
            advanced
                .pending_resume_progress
                .suppressed_notification_calls_delta,
            1
        );
        assert_eq!(advanced.pending_resume_progress.skipped_yields_delta, 1);
        assert_eq!(
            advanced
                .pending_resume_progress
                .total_nested_tool_calls_delta,
            3
        );
    }

    #[test]
    fn test_cell_drain_snapshot_to_exec_result_preserves_yielded_state() {
        let snapshot = CellDrainSnapshot {
            status: CellStatus::Running,
            nested_tool_calls: 2,
            render_state: DrainRenderState {
                output_text: "hello".to_string(),
                notifications: vec!["done".to_string()],
                yield_value: Some(serde_json::json!("pause")),
                yield_kind: Some(ExecYieldKind::Manual),
                ..DrainRenderState::default()
            },
            truncated: true,
        };

        let result = snapshot.to_exec_result("cell_3");

        assert_eq!(result.cell_id, "cell_3");
        assert!(result.yielded);
        assert_eq!(result.yield_kind, Some(ExecYieldKind::Manual));
        assert_eq!(result.yield_value, Some(serde_json::json!("pause")));
        assert_eq!(result.return_value, None);
        assert_eq!(result.output_text, "hello");
        assert_eq!(result.notifications, vec!["done".to_string()]);
        assert_eq!(result.nested_tool_calls, 2);
        assert!(result.truncated);
    }

    #[test]
    fn test_cell_drain_snapshot_to_exec_result_preserves_completed_state() {
        let snapshot = CellDrainSnapshot {
            status: CellStatus::Completed,
            nested_tool_calls: 1,
            render_state: DrainRenderState {
                output_text: "done".to_string(),
                return_value: Some(serde_json::json!({ "ok": true })),
                ..DrainRenderState::default()
            },
            truncated: false,
        };

        let result = snapshot.to_exec_result("cell_4");

        assert!(!result.yielded);
        assert_eq!(result.yield_kind, None);
        assert_eq!(result.yield_value, None);
        assert_eq!(result.return_value, Some(serde_json::json!({ "ok": true })));
        assert_eq!(result.output_text, "done");
    }

    #[test]
    fn test_bounded_recent_events_preserves_tail_and_marks_truncation() {
        let events = vec![
            RuntimeEvent::Text {
                seq: 1,
                chunk: "x".repeat(RECENT_EVENT_BUDGET_CHARS + 1),
            },
            RuntimeEvent::Yield {
                seq: 2,
                kind: ExecYieldKind::Manual,
                value: Some(serde_json::json!("pause")),
                resume_after_ms: None,
            },
        ];
        let (recent_events, recent_events_truncated) = bounded_recent_events(events);
        let active_cell = ActiveCellHandle {
            cell_id: "cell_3".to_string(),
            code: "yield_control()".to_string(),
            visible_tools: Vec::new(),
            status: CellStatus::Running,
            last_event_seq: 2,
            recent_events,
            recent_events_truncated,
            resume_state: CellResumeState {
                total_nested_tool_calls: 1,
                ..CellResumeState::default()
            },
            pending_resume_progress: CellResumeProgressDelta::default(),
        };

        assert!(active_cell.recent_events_truncated);
        assert_eq!(active_cell.recent_events.len(), 1);
        assert!(matches!(
            active_cell.recent_events.first(),
            Some(RuntimeEvent::Yield {
                kind: ExecYieldKind::Manual,
                ..
            })
        ));
        assert!(active_cell
            .render_recent_events(false)
            .contains("[output truncated to stay within the code-mode budget]"));
    }

    #[test]
    fn test_active_cell_renders_waiting_on_tool_snapshot_without_events() {
        let active_cell = ActiveCellHandle {
            cell_id: "cell_4".to_string(),
            code: "await tools.echo_tool({ value: \"hello\" })".to_string(),
            visible_tools: vec!["echo_tool".to_string()],
            status: CellStatus::WaitingOnTool { request_id: 9 },
            last_event_seq: 0,
            recent_events: Vec::new(),
            recent_events_truncated: false,
            resume_state: CellResumeState {
                total_nested_tool_calls: 3,
                ..CellResumeState::default()
            },
            pending_resume_progress: CellResumeProgressDelta::default(),
        };

        let rendered = active_cell.render_recent_events(false);

        assert!(rendered.contains("waiting on nested tool request 9"));
        assert!(rendered.contains("after 3 nested tool call(s)"));
    }
}
