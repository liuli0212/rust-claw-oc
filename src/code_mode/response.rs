use super::cell::CellStatus;
use super::protocol::RuntimeEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecOutputItem {
    Text(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecRunResult {
    pub cell_id: String,
    pub output_text: String,
    pub return_value: Option<Value>,
    pub flush_value: Option<Value>,
    pub flushed: bool,
    pub waiting_on_timer_ms: Option<u64>,
    pub notifications: Vec<String>,
    pub failure: Option<String>,
    pub cancellation: Option<String>,
    pub nested_tool_calls: usize,
    pub truncated: bool,
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
    pub flush_value: Option<Value>,
    pub failure: Option<String>,
    pub cancellation: Option<String>,
    pub waiting_on_timer_ms: Option<u64>,
    pub is_flushed: bool,
}

impl DrainRenderState {
    pub fn from_exec_result(result: &ExecRunResult) -> Self {
        Self {
            output_text: result.output_text.clone(),
            notifications: result.notifications.clone(),
            return_value: result.return_value.clone(),
            flush_value: result.flush_value.clone(),
            waiting_on_timer_ms: result.waiting_on_timer_ms,
            failure: result.failure.clone(),
            cancellation: result.cancellation.clone(),
            is_flushed: result.flushed,
        }
    }

    pub fn from_events(events: &[RuntimeEvent]) -> Self {
        let mut state = Self::default();

        for event in events {
            match event {
                RuntimeEvent::Text { text, .. } => {
                    state.waiting_on_timer_ms = None;
                    if !state.output_text.is_empty() && !text.is_empty() {
                        state.output_text.push('\n');
                    }
                    state.output_text.push_str(text);
                }
                RuntimeEvent::Notification { message, .. } => {
                    state.waiting_on_timer_ms = None;
                    state.notifications.push(message.clone());
                }
                RuntimeEvent::Flush { value, .. } => {
                    state.flush_value = value.clone();
                    state.waiting_on_timer_ms = None;
                    state.is_flushed = true;
                }
                RuntimeEvent::WaitingForTimer {
                    resume_after_ms, ..
                } => {
                    state.waiting_on_timer_ms = *resume_after_ms;
                    state.is_flushed = true;
                }
                RuntimeEvent::Completed { return_value, .. } => {
                    state.return_value = return_value.clone();
                    state.flush_value = None;
                    state.waiting_on_timer_ms = None;
                    state.is_flushed = false;
                }
                RuntimeEvent::Failed { error, .. } => {
                    state.failure = Some(error.clone());
                    state.cancellation = None;
                    state.waiting_on_timer_ms = None;
                }
                RuntimeEvent::Cancelled { reason, .. } => {
                    state.cancellation = Some(reason.clone());
                    state.failure = None;
                    state.waiting_on_timer_ms = None;
                }
                RuntimeEvent::ToolCallRequested(_) | RuntimeEvent::ToolCallResolved { .. } => {
                    state.waiting_on_timer_ms = None;
                }
                RuntimeEvent::WorkerCompleted(_)
                | RuntimeEvent::TimerRegistrationChanged { .. } => {}
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
            let value_label = if self.flush_value.is_some() {
                "Flush value:"
            } else {
                "Return value:"
            };
            let value_to_render = if self.flush_value.is_some() {
                self.flush_value.as_ref()
            } else {
                self.return_value.as_ref()
            };

            if let Some(value) = value_to_render {
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
            CellStatus::Starting | CellStatus::Running | CellStatus::Flushed => {
                Some(format!(
                    "Code mode cell `{}` is still running after {} nested tool call(s). Call `wait` to check for more output.",
                    cell_id, nested_tool_calls
                ))
            }
            CellStatus::WaitingOnTool { request_id } => Some(format!(
                "Code mode cell `{}` is processing nested tool request {} after {} nested tool call(s). Call `wait` to observe more progress.",
                cell_id, request_id, nested_tool_calls
            )),
            CellStatus::WaitingOnJsTimer { next_due_in_ms } => {
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
            CellStatus::Completed => {
                if self.cancellation.is_some() || self.failure.is_some() {
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
        } else if let Some(resume_after_ms) = self.waiting_on_timer_ms {
            format!(
                "Code mode cell `{}` is waiting on timer(s) after {} nested tool call(s). Call `wait` again in about {} ms.",
                cell_id, nested_tool_calls, resume_after_ms
            )
        } else if self.is_flushed {
            format!(
                "Code mode cell `{}` flushed after {} nested tool call(s). Call `wait` to resume it.",
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
    fn render_output_preserves_failure_details() {
        let summary = ExecRunResult {
            cell_id: "cell-7".to_string(),
            output_text: "partial output".to_string(),
            return_value: None,
            flush_value: None,
            flushed: false,
            waiting_on_timer_ms: None,
            notifications: Vec::new(),
            failure: Some("ReferenceError: boom".to_string()),
            cancellation: None,
            nested_tool_calls: 2,
            truncated: false,
        };

        let rendered = summary.render_output();
        assert!(rendered.contains("failed after 2 nested tool call(s)"));
        assert!(rendered.contains("Failure:\nReferenceError: boom"));
        assert!(rendered.contains("Text output:\npartial output"));
    }

    #[test]
    fn render_output_preserves_cancellation_details() {
        let summary = ExecRunResult {
            cell_id: "cell-8".to_string(),
            output_text: String::new(),
            return_value: None,
            flush_value: None,
            flushed: false,
            waiting_on_timer_ms: None,
            notifications: Vec::new(),
            failure: None,
            cancellation: Some("interrupted by user".to_string()),
            nested_tool_calls: 0,
            truncated: false,
        };

        let rendered = summary.render_output();
        assert!(rendered.contains("was cancelled"));
        assert!(rendered.contains("Cancellation reason:\ninterrupted by user"));
    }

    #[test]
    fn render_output_uses_timer_wait_status_for_flushed_cells() {
        let summary = ExecRunResult {
            cell_id: "cell-9".to_string(),
            output_text: String::new(),
            return_value: None,
            flush_value: None,
            flushed: true,
            waiting_on_timer_ms: Some(125),
            notifications: Vec::new(),
            failure: None,
            cancellation: None,
            nested_tool_calls: 1,
            truncated: false,
        };

        let rendered = summary.render_output();
        assert!(rendered.contains("waiting on timer(s)"));
        assert!(rendered.contains("about 125 ms"));
        assert!(!rendered.contains("flushed after 1 nested tool call(s)"));
    }
}
