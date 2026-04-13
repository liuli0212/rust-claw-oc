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

impl ExecRunResult {
    fn timer_wait_details(&self) -> Option<(usize, Option<u64>)> {
        let yield_value = self.yield_value.as_ref()?;
        let obj = yield_value.as_object()?;
        if obj.get("reason").and_then(Value::as_str) != Some("timer_pending") {
            return None;
        }

        let pending_timers = obj
            .get("pending_timers")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(0);
        let resume_after_ms = obj.get("resume_after_ms").and_then(Value::as_u64);
        Some((pending_timers, resume_after_ms))
    }

    pub fn render_output(&self) -> String {
        let status_line = if self.yielded {
            if matches!(self.yield_kind, Some(ExecYieldKind::Timer)) {
                let (pending_timers, resume_after_ms) =
                    self.timer_wait_details().unwrap_or((0, None));
                match resume_after_ms {
                    Some(delay) => format!(
                        "Code mode cell `{}` is waiting on {} timer(s) after {} nested tool call(s). Call `wait` again in about {} ms.",
                        self.cell_id, pending_timers, self.nested_tool_calls, delay
                    ),
                    None => format!(
                        "Code mode cell `{}` is waiting on {} timer(s) after {} nested tool call(s). Call `wait` to resume it.",
                        self.cell_id, pending_timers, self.nested_tool_calls
                    ),
                }
            } else {
                format!(
                    "Code mode cell `{}` yielded after {} nested tool call(s). Call `wait` to resume it.",
                    self.cell_id, self.nested_tool_calls
                )
            }
        } else {
            format!(
                "Code mode cell `{}` completed after {} nested tool call(s).",
                self.cell_id, self.nested_tool_calls
            )
        };
        let mut lines = vec![status_line];

        if !self.output_text.trim().is_empty() {
            lines.push("Text output:".to_string());
            lines.push(self.output_text.trim().to_string());
        }

        let value_label = if self.yielded {
            "Yield value:"
        } else {
            "Return value:"
        };
        let value_to_render = if self.yielded {
            self.yield_value.as_ref()
        } else {
            self.return_value.as_ref()
        };

        if self.yielded && matches!(self.yield_kind, Some(ExecYieldKind::Timer)) {
            // Timer-driven yields already surface their scheduling metadata in the status line.
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

        if !self.notifications.is_empty() {
            lines.push("Notifications:".to_string());
            lines.extend(
                self.notifications
                    .iter()
                    .map(|item| format!("- {item}"))
                    .collect::<Vec<_>>(),
            );
        }

        if self.truncated {
            lines.push("[output truncated to stay within the code-mode budget]".to_string());
        }

        lines.join("\n")
    }
}
