use super::cell::CellStatus;
use super::protocol::RuntimeEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecOutputItem {
    Text(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ExecYieldKind {
    Manual,
    Timer,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecRunResult {
    pub cell_id: String,
    pub output_text: String,
    pub return_value: Option<Value>,
    pub yield_value: Option<Value>,
    pub yielded: bool,
    pub yield_kind: Option<ExecYieldKind>,
    pub notifications: Vec<String>,
    pub nested_tool_calls: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerPendingDetails {
    pub pending_timers: usize,
    pub resume_after_ms: Option<u64>,
}

pub fn timer_pending_details(yield_value: Option<&Value>) -> Option<TimerPendingDetails> {
    let obj = yield_value?.as_object()?;
    if obj.get("reason").and_then(Value::as_str) != Some("timer_pending") {
        return None;
    }

    Some(TimerPendingDetails {
        pending_timers: obj
            .get("pending_timers")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(0),
        resume_after_ms: obj.get("resume_after_ms").and_then(Value::as_u64),
    })
}

pub fn timer_pending_resume_after_ms(yield_value: Option<&Value>) -> Option<u64> {
    timer_pending_details(yield_value).and_then(|details| details.resume_after_ms)
}

impl ExecRunResult {
    pub fn render_output(&self) -> String {
        DrainRenderState::from_exec_result(self).render_output(
            &self.cell_id,
            self.nested_tool_calls,
            self.truncated,
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DrainRenderState {
    pub output_text: String,
    pub notifications: Vec<String>,
    pub return_value: Option<Value>,
    pub yield_value: Option<Value>,
    pub yield_kind: Option<ExecYieldKind>,
    pub failure: Option<String>,
    pub cancellation: Option<String>,
}

impl DrainRenderState {
    pub fn from_exec_result(result: &ExecRunResult) -> Self {
        Self {
            output_text: result.output_text.clone(),
            notifications: result.notifications.clone(),
            return_value: result.return_value.clone(),
            yield_value: result.yield_value.clone(),
            yield_kind: result.yield_kind.clone(),
            failure: None,
            cancellation: None,
        }
    }

    pub fn from_events(events: &[RuntimeEvent]) -> Self {
        let mut state = Self::default();

        for event in events {
            match event {
                RuntimeEvent::Text { chunk, .. } => {
                    if !state.output_text.is_empty() && !chunk.is_empty() {
                        state.output_text.push('\n');
                    }
                    state.output_text.push_str(chunk);
                }
                RuntimeEvent::Notification { message, .. } => {
                    state.notifications.push(message.clone());
                }
                RuntimeEvent::Yield { kind, value, .. } => {
                    state.yield_kind = Some(kind.clone());
                    state.yield_value = value.clone();
                    state.return_value = None;
                    state.failure = None;
                    state.cancellation = None;
                }
                RuntimeEvent::Completed { return_value, .. } => {
                    state.return_value = return_value.clone();
                    state.yield_kind = None;
                    state.yield_value = None;
                    state.failure = None;
                    state.cancellation = None;
                }
                RuntimeEvent::Failed { error, .. } => {
                    state.failure = Some(error.clone());
                    state.cancellation = None;
                }
                RuntimeEvent::Cancelled { reason, .. } => {
                    state.cancellation = Some(reason.clone());
                    state.failure = None;
                }
                RuntimeEvent::ToolCallRequested(_)
                | RuntimeEvent::ToolCallResolved { .. }
                | RuntimeEvent::WorkerCompleted(_) => {}
            }
        }

        state
    }

    pub fn render_output(
        &self,
        cell_id: &str,
        nested_tool_calls: usize,
        truncated: bool,
    ) -> String {
        self.render_output_with_status(cell_id, nested_tool_calls, truncated, None)
    }

    pub fn render_output_with_status(
        &self,
        cell_id: &str,
        nested_tool_calls: usize,
        truncated: bool,
        status: Option<&CellStatus>,
    ) -> String {
        let status_line = status
            .and_then(|status| self.status_line_from_status(cell_id, nested_tool_calls, status))
            .unwrap_or_else(|| self.default_status_line(cell_id, nested_tool_calls));

        let mut lines = vec![status_line];

        if !self.output_text.trim().is_empty() {
            lines.push("Text output:".to_string());
            lines.push(self.output_text.trim().to_string());
        }

        if let Some(reason) = &self.cancellation {
            if !reason.trim().is_empty() {
                lines.push("Cancellation reason:".to_string());
                lines.push(reason.clone());
            }
        } else if let Some(error) = &self.failure {
            if !error.trim().is_empty() {
                lines.push("Failure:".to_string());
                lines.push(error.clone());
            }
        } else {
            let value_label = if self.yield_kind.is_some() {
                "Yield value:"
            } else {
                "Return value:"
            };
            let value_to_render = if self.yield_kind.is_some() {
                self.yield_value.as_ref()
            } else {
                self.return_value.as_ref()
            };

            if self.yield_kind == Some(ExecYieldKind::Timer) {
            } else if let Some(value) = value_to_render {
                let rendered = if value.is_string() {
                    value.as_str().unwrap_or_default().to_string()
                } else {
                    value.to_string()
                };
                if !rendered.trim().is_empty() && rendered != "null" {
                    lines.push(value_label.to_string());
                    lines.push(crate::context::AgentContext::truncate_chars(
                        &rendered, 4_000,
                    ));
                }
            }
        }

        if !self.notifications.is_empty() {
            lines.push("Notifications:".to_string());
            lines.extend(
                self.notifications
                    .iter()
                    .map(|item| format!("- {item}"))
                    .collect::<Vec<_>>(),
            );
        }

        if truncated {
            lines.push("[output truncated to stay within the code-mode budget]".to_string());
        }

        lines.join("\n")
    }

    fn status_line_from_status(
        &self,
        cell_id: &str,
        nested_tool_calls: usize,
        status: &CellStatus,
    ) -> Option<String> {
        match status {
            CellStatus::Starting | CellStatus::Running => {
                if self.yield_kind.is_some() {
                    None
                } else {
                    Some(format!(
                        "Code mode cell `{}` is running after {} nested tool call(s). Call `wait` to resume it.",
                        cell_id, nested_tool_calls
                    ))
                }
            }
            CellStatus::WaitingOnTool { request_id } => Some(format!(
                "Code mode cell `{}` is waiting on nested tool request {} after {} nested tool call(s). Call `wait` to resume it.",
                cell_id, request_id, nested_tool_calls
            )),
            CellStatus::WaitingOnJsTimer { next_due_in_ms } => {
                if matches!(self.yield_kind, Some(ExecYieldKind::Timer)) {
                    None
                } else {
                    Some(match next_due_in_ms {
                        Some(delay) => format!(
                            "Code mode cell `{}` is waiting on a timer after {} nested tool call(s). Call `wait` again in about {} ms.",
                            cell_id, nested_tool_calls, delay
                        ),
                        None => format!(
                            "Code mode cell `{}` is waiting on a timer after {} nested tool call(s). Call `wait` to resume it.",
                            cell_id, nested_tool_calls
                        ),
                    })
                }
            }
            CellStatus::Completed => {
                if self.cancellation.is_some() || self.failure.is_some() || self.yield_kind.is_some() {
                    None
                } else {
                    Some(format!(
                        "Code mode cell `{}` completed after {} nested tool call(s).",
                        cell_id, nested_tool_calls
                    ))
                }
            }
            CellStatus::Failed => {
                if self.failure.is_some() {
                    None
                } else {
                    Some(format!(
                        "Code mode cell `{}` failed after {} nested tool call(s).",
                        cell_id, nested_tool_calls
                    ))
                }
            }
            CellStatus::Cancelled => {
                if self.cancellation.is_some() {
                    None
                } else {
                    Some(format!(
                        "Code mode cell `{}` was cancelled after {} nested tool call(s).",
                        cell_id, nested_tool_calls
                    ))
                }
            }
        }
    }

    fn default_status_line(&self, cell_id: &str, nested_tool_calls: usize) -> String {
        if self.cancellation.is_some() {
            format!(
                "Code mode cell `{}` was cancelled after {} nested tool call(s).",
                cell_id, nested_tool_calls
            )
        } else if self.failure.is_some() {
            format!(
                "Code mode cell `{}` failed after {} nested tool call(s).",
                cell_id, nested_tool_calls
            )
        } else if matches!(self.yield_kind, Some(ExecYieldKind::Timer)) {
            let TimerPendingDetails {
                pending_timers,
                resume_after_ms,
            } = timer_pending_details(self.yield_value.as_ref()).unwrap_or(TimerPendingDetails {
                pending_timers: 0,
                resume_after_ms: None,
            });
            match resume_after_ms {
                Some(delay) => format!(
                    "Code mode cell `{}` is waiting on {} timer(s) after {} nested tool call(s). Call `wait` again in about {} ms.",
                    cell_id, pending_timers, nested_tool_calls, delay
                ),
                None => format!(
                    "Code mode cell `{}` is waiting on {} timer(s) after {} nested tool call(s). Call `wait` to resume it.",
                    cell_id, pending_timers, nested_tool_calls
                ),
            }
        } else if self.yield_kind.is_some() {
            format!(
                "Code mode cell `{}` yielded after {} nested tool call(s). Call `wait` to resume it.",
                cell_id, nested_tool_calls
            )
        } else {
            format!(
                "Code mode cell `{}` completed after {} nested tool call(s).",
                cell_id, nested_tool_calls
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_drain_render_state_renders_completed_event_slice() {
        let events = vec![
            RuntimeEvent::Text {
                seq: 1,
                chunk: "hello".to_string(),
            },
            RuntimeEvent::Notification {
                seq: 2,
                message: "done".to_string(),
            },
            RuntimeEvent::Completed {
                seq: 3,
                return_value: Some(serde_json::json!({ "ok": true })),
            },
        ];

        let state = DrainRenderState::from_events(&events);
        let rendered = state.render_output("cell_1", 2, false);

        assert_eq!(state.output_text, "hello");
        assert_eq!(state.notifications, vec!["done".to_string()]);
        assert_eq!(state.return_value, Some(serde_json::json!({ "ok": true })));
        assert!(rendered.contains("completed after 2 nested tool call(s)"));
        assert!(rendered.contains("Text output:"));
        assert!(rendered.contains("Return value:"));
        assert!(rendered.contains("Notifications:"));
    }

    #[test]
    fn test_drain_render_state_renders_timer_yield_event_slice() {
        let events = vec![
            RuntimeEvent::Text {
                seq: 1,
                chunk: "before\nafter".to_string(),
            },
            RuntimeEvent::Yield {
                seq: 2,
                kind: ExecYieldKind::Timer,
                value: Some(serde_json::json!({
                    "reason": "timer_pending",
                    "pending_timers": 1,
                    "resume_after_ms": 20
                })),
                resume_after_ms: Some(20),
            },
        ];

        let state = DrainRenderState::from_events(&events);
        let rendered = state.render_output("cell_2", 0, false);

        assert_eq!(state.yield_kind, Some(ExecYieldKind::Timer));
        assert!(rendered.contains("waiting on 1 timer(s)"));
        assert!(rendered.contains("about 20 ms"));
        assert!(rendered.contains("Text output:"));
        assert!(!rendered.contains("Yield value:"));
    }

    #[test]
    fn test_exec_result_render_matches_completed_event_render() {
        let result = ExecRunResult {
            cell_id: "cell_3".to_string(),
            output_text: "hello".to_string(),
            return_value: Some(serde_json::json!({ "ok": true })),
            yield_value: None,
            yielded: false,
            yield_kind: None,
            notifications: vec!["done".to_string()],
            nested_tool_calls: 2,
            truncated: false,
        };
        let events = vec![
            RuntimeEvent::Text {
                seq: 1,
                chunk: "hello".to_string(),
            },
            RuntimeEvent::Notification {
                seq: 2,
                message: "done".to_string(),
            },
            RuntimeEvent::Completed {
                seq: 3,
                return_value: Some(serde_json::json!({ "ok": true })),
            },
        ];

        let summary_render = result.render_output();
        let event_render = DrainRenderState::from_events(&events).render_output("cell_3", 2, false);

        assert_eq!(summary_render, event_render);
    }

    #[test]
    fn test_exec_result_render_matches_timer_yield_event_render() {
        let result = ExecRunResult {
            cell_id: "cell_4".to_string(),
            output_text: "before\nafter".to_string(),
            return_value: None,
            yield_value: Some(serde_json::json!({
                "reason": "timer_pending",
                "pending_timers": 1,
                "resume_after_ms": 20
            })),
            yielded: true,
            yield_kind: Some(ExecYieldKind::Timer),
            notifications: Vec::new(),
            nested_tool_calls: 0,
            truncated: false,
        };
        let events = vec![
            RuntimeEvent::Text {
                seq: 1,
                chunk: "before\nafter".to_string(),
            },
            RuntimeEvent::Yield {
                seq: 2,
                kind: ExecYieldKind::Timer,
                value: result.yield_value.clone(),
                resume_after_ms: Some(20),
            },
        ];

        let summary_render = result.render_output();
        let event_render = DrainRenderState::from_events(&events).render_output("cell_4", 0, false);

        assert_eq!(summary_render, event_render);
    }

    #[test]
    fn test_drain_render_state_renders_waiting_on_tool_snapshot_without_events() {
        let rendered = DrainRenderState::default().render_output_with_status(
            "cell_5",
            3,
            false,
            Some(&CellStatus::WaitingOnTool { request_id: 7 }),
        );

        assert!(rendered.contains("waiting on nested tool request 7"));
        assert!(rendered.contains("after 3 nested tool call(s)"));
    }

    #[test]
    fn test_drain_render_state_renders_waiting_on_timer_snapshot_without_events() {
        let rendered = DrainRenderState::default().render_output_with_status(
            "cell_6",
            1,
            false,
            Some(&CellStatus::WaitingOnJsTimer {
                next_due_in_ms: Some(20),
            }),
        );

        assert!(rendered.contains("waiting on a timer"));
        assert!(rendered.contains("about 20 ms"));
    }

    #[test]
    fn test_timer_pending_details_extracts_pending_count_and_delay() {
        let details = timer_pending_details(Some(&serde_json::json!({
            "reason": "timer_pending",
            "pending_timers": 2,
            "resume_after_ms": 25
        })))
        .expect("timer-pending metadata is parsed");

        assert_eq!(details.pending_timers, 2);
        assert_eq!(details.resume_after_ms, Some(25));
        assert_eq!(
            timer_pending_resume_after_ms(Some(&serde_json::json!({
                "reason": "timer_pending",
                "pending_timers": 2,
                "resume_after_ms": 25
            }))),
            Some(25)
        );
    }
}
