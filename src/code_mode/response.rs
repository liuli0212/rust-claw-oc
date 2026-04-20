use super::protocol::RuntimeEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecOutputItem {
    Text(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecLifecycle {
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecProgressKind {
    ExplicitFlush,
    AutoFlush,
}

impl Default for ExecLifecycle {
    fn default() -> Self {
        Self::Running
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecRunResult {
    pub cell_id: String,
    pub output_text: String,
    pub return_value: Option<Value>,
    pub flush_value: Option<Value>,
    pub lifecycle: ExecLifecycle,
    pub progress_kind: Option<ExecProgressKind>,
    pub flushed: bool,
    pub waiting_on_tool_request_id: Option<String>,
    pub waiting_on_timer_ms: Option<u64>,
    pub notifications: Vec<String>,
    pub failure: Option<String>,
    pub cancellation: Option<String>,
    pub nested_tool_calls: usize,
    pub truncated: bool,
}

impl ExecRunResult {
    pub fn render_output(&self) -> String {
        CellRenderState::from_exec_result(self).render_output(
            &self.cell_id,
            self.nested_tool_calls,
            self.truncated,
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CellRenderState {
    pub output_text: String,
    pub notifications: Vec<String>,
    pub return_value: Option<Value>,
    pub flush_value: Option<Value>,
    pub lifecycle: ExecLifecycle,
    pub progress_kind: Option<ExecProgressKind>,
    pub waiting_on_tool_request_id: Option<String>,
    pub waiting_on_timer_ms: Option<u64>,
    pub failure: Option<String>,
    pub cancellation: Option<String>,
}

impl CellRenderState {
    pub fn from_exec_result(result: &ExecRunResult) -> Self {
        Self {
            output_text: result.output_text.clone(),
            notifications: result.notifications.clone(),
            return_value: result.return_value.clone(),
            flush_value: result.flush_value.clone(),
            lifecycle: result.lifecycle.clone(),
            progress_kind: result.progress_kind.clone(),
            waiting_on_tool_request_id: result.waiting_on_tool_request_id.clone(),
            waiting_on_timer_ms: result.waiting_on_timer_ms,
            failure: result.failure.clone(),
            cancellation: result.cancellation.clone(),
        }
    }

    pub fn from_events(events: &[RuntimeEvent]) -> Self {
        let mut state = Self::default();

        for event in events {
            match event {
                RuntimeEvent::Text { text, .. } => {
                    state.flush_value = None;
                    if !state.output_text.is_empty() && !text.is_empty() {
                        state.output_text.push('\n');
                    }
                    state.output_text.push_str(text);
                }
                RuntimeEvent::Notification { message, .. } => {
                    state.flush_value = None;
                    state.notifications.push(message.clone());
                }
                RuntimeEvent::Flush { value, .. } => {
                    state.flush_value = value.clone();
                }
                RuntimeEvent::ToolCallRequested(_)
                | RuntimeEvent::ToolCallResolved { .. }
                | RuntimeEvent::WaitingForTimer { .. }
                | RuntimeEvent::WorkerCompleted(_)
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
        let mut lines = vec![self.default_status_line(cell_id, nested_tool_calls)];

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

    fn default_status_line(&self, cell_id: &str, nested_tool_calls: usize) -> String {
        match &self.lifecycle {
            ExecLifecycle::Cancelled => format!(
                "Code mode cell `{}` was cancelled after {} nested tool call(s).",
                cell_id, nested_tool_calls
            ),
            ExecLifecycle::Failed => format!(
                "Code mode cell `{}` failed after {} nested tool call(s).",
                cell_id, nested_tool_calls
            ),
            ExecLifecycle::Completed => format!(
                "Code mode cell `{}` completed after {} nested tool call(s).",
                cell_id, nested_tool_calls
            ),
            ExecLifecycle::Running
                if self.progress_kind.as_ref() == Some(&ExecProgressKind::ExplicitFlush) =>
            {
                format!(
                    "Code mode cell `{}` flushed after {} nested tool call(s). Call `wait` to sync more output.",
                    cell_id, nested_tool_calls
                )
            }
            ExecLifecycle::Running
                if self.progress_kind.as_ref() == Some(&ExecProgressKind::AutoFlush) =>
            {
                format!(
                    "Code mode cell `{}` published an automatic progress update after {} nested tool call(s). Call `wait` to sync more output.",
                    cell_id, nested_tool_calls
                )
            }
            ExecLifecycle::Running if self.waiting_on_tool_request_id.is_some() => format!(
                "Code mode cell `{}` is processing nested tool request {} after {} nested tool call(s). Call `wait` to poll for more output.",
                cell_id,
                self.waiting_on_tool_request_id.as_deref().unwrap_or("unknown"),
                nested_tool_calls
            ),
            ExecLifecycle::Running if self.waiting_on_timer_ms.is_some() => format!(
                "Code mode cell `{}` is still running in the background after {} nested tool call(s). Call `wait` to poll for more output.",
                cell_id, nested_tool_calls
            ),
            ExecLifecycle::Running => format!(
                "Code mode cell `{}` is still running after {} nested tool call(s). Call `wait` to poll for more output.",
                cell_id, nested_tool_calls
            ),
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
            lifecycle: ExecLifecycle::Failed,
            progress_kind: None,
            flushed: false,
            waiting_on_tool_request_id: None,
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
            lifecycle: ExecLifecycle::Cancelled,
            progress_kind: None,
            flushed: false,
            waiting_on_tool_request_id: None,
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
    fn render_output_uses_progress_status_for_auto_flush_cells() {
        let summary = ExecRunResult {
            cell_id: "cell-9".to_string(),
            output_text: String::new(),
            return_value: None,
            flush_value: None,
            lifecycle: ExecLifecycle::Running,
            progress_kind: Some(ExecProgressKind::AutoFlush),
            flushed: true,
            waiting_on_tool_request_id: None,
            waiting_on_timer_ms: Some(125),
            notifications: Vec::new(),
            failure: None,
            cancellation: None,
            nested_tool_calls: 1,
            truncated: false,
        };

        let rendered = summary.render_output();
        assert!(rendered.contains("automatic progress update"));
        assert!(!rendered.contains("waiting on timer"));
    }

    #[test]
    fn render_output_uses_waiting_tool_status_for_running_cells() {
        let summary = ExecRunResult {
            cell_id: "cell-10".to_string(),
            output_text: String::new(),
            return_value: None,
            flush_value: None,
            lifecycle: ExecLifecycle::Running,
            progress_kind: None,
            flushed: false,
            waiting_on_tool_request_id: Some("echo-3".to_string()),
            waiting_on_timer_ms: None,
            notifications: Vec::new(),
            failure: None,
            cancellation: None,
            nested_tool_calls: 3,
            truncated: false,
        };

        let rendered = summary.render_output();
        assert!(rendered.contains("processing nested tool request echo-3"));
    }
}
