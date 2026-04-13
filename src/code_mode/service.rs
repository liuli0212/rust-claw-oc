use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::response::{ExecRunResult, ExecYieldKind};
use super::runtime;
use super::runtime::callbacks::RecordedToolCall;
use super::runtime::timers::RecordedTimerCall;

#[derive(Debug, Default, Clone)]
pub struct CodeModeService {
    sessions: Arc<Mutex<HashMap<String, SessionState>>>,
}

#[derive(Debug, Default, Clone)]
struct SessionState {
    next_cell_seq: u64,
    stored_values: HashMap<String, serde_json::Value>,
    pending_cell: Option<PendingCellState>,
}

#[derive(Debug, Clone)]
struct PendingCellState {
    cell_id: String,
    code: String,
    visible_tools: Vec<String>,
    replayed_tool_calls: Vec<RecordedToolCall>,
    recorded_timer_calls: Vec<RecordedTimerCall>,
    suppressed_text_calls: usize,
    suppressed_notification_calls: usize,
    skipped_yields: usize,
    total_nested_tool_calls: usize,
}

struct ToolBridgeRequest {
    tool_name: String,
    args_json: String,
    response_tx: std::sync::mpsc::SyncSender<Result<String, crate::tools::ToolError>>,
}

impl CodeModeService {
    pub async fn execute<F, Fut>(
        &self,
        session_id: &str,
        code: &str,
        visible_tools: Vec<String>,
        invoke_tool: &mut F,
    ) -> Result<ExecRunResult, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        let (cell_id, stored_values) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions.entry(session_id.to_string()).or_default();
            if let Some(pending) = &session.pending_cell {
                return Err(crate::tools::ToolError::ExecutionFailed(format!(
                    "Code mode cell `{}` is still running. Call `wait` to resume it before starting another `exec`.",
                    pending.cell_id
                )));
            }
            session.next_cell_seq += 1;
            (
                format!("cell_{}", session.next_cell_seq),
                session.stored_values.clone(),
            )
        };

        let result = self
            .run_runtime_cell(
                cell_id.clone(),
                code.to_string(),
                visible_tools.clone(),
                stored_values,
                runtime::ResumeState::default(),
                invoke_tool,
            )
            .await?;

        let (mut summary, stored_values, metadata) = result;
        let runtime::RunCellMetadata {
            total_text_calls,
            total_notification_calls,
            newly_recorded_tool_calls,
            timer_calls,
        } = metadata;
        let mut sessions = self.sessions.lock().await;
        let session = sessions.entry(session_id.to_string()).or_default();
        session.stored_values = stored_values;
        if summary.yielded {
            session.pending_cell = Some(PendingCellState {
                cell_id: cell_id.clone(),
                code: code.to_string(),
                visible_tools,
                replayed_tool_calls: newly_recorded_tool_calls,
                recorded_timer_calls: timer_calls,
                suppressed_text_calls: total_text_calls,
                suppressed_notification_calls: total_notification_calls,
                skipped_yields: if matches!(summary.yield_kind, Some(ExecYieldKind::Manual)) {
                    1
                } else {
                    0
                },
                total_nested_tool_calls: summary.nested_tool_calls,
            });
        } else {
            session.pending_cell = None;
        }
        summary.cell_id = cell_id;
        Ok(summary)
    }

    pub async fn wait<F, Fut>(
        &self,
        session_id: &str,
        requested_cell_id: Option<&str>,
        invoke_tool: &mut F,
    ) -> Result<ExecRunResult, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        let (pending, stored_values) = {
            let sessions = self.sessions.lock().await;
            let session = sessions.get(session_id).ok_or_else(|| {
                crate::tools::ToolError::ExecutionFailed(
                    "No pending code mode cell is available for this session.".to_string(),
                )
            })?;
            let pending = session.pending_cell.clone().ok_or_else(|| {
                crate::tools::ToolError::ExecutionFailed(
                    "No pending code mode cell is available for this session.".to_string(),
                )
            })?;
            if let Some(cell_id) = requested_cell_id {
                if cell_id != pending.cell_id {
                    return Err(crate::tools::ToolError::ExecutionFailed(format!(
                        "Pending code mode cell mismatch: expected `{}`, got `{}`.",
                        pending.cell_id, cell_id
                    )));
                }
            }
            (pending, session.stored_values.clone())
        };

        let result = self
            .run_runtime_cell(
                pending.cell_id.clone(),
                pending.code.clone(),
                pending.visible_tools.clone(),
                stored_values,
                runtime::ResumeState {
                    replayed_tool_calls: pending.replayed_tool_calls.clone(),
                    recorded_timer_calls: pending.recorded_timer_calls.clone(),
                    skipped_yields: pending.skipped_yields,
                    suppressed_text_calls: pending.suppressed_text_calls,
                    suppressed_notification_calls: pending.suppressed_notification_calls,
                },
                invoke_tool,
            )
            .await;

        match result {
            Ok((mut summary, stored_values, metadata)) => {
                let runtime::RunCellMetadata {
                    total_text_calls,
                    total_notification_calls,
                    newly_recorded_tool_calls,
                    timer_calls,
                } = metadata;
                let total_nested_tool_calls =
                    pending.total_nested_tool_calls + summary.nested_tool_calls;
                summary.nested_tool_calls = total_nested_tool_calls;

                let mut sessions = self.sessions.lock().await;
                let session = sessions.entry(session_id.to_string()).or_default();
                session.stored_values = stored_values;

                if summary.yielded {
                    let mut replayed_tool_calls = pending.replayed_tool_calls;
                    replayed_tool_calls.extend(newly_recorded_tool_calls);
                    session.pending_cell = Some(PendingCellState {
                        cell_id: pending.cell_id,
                        code: pending.code,
                        visible_tools: pending.visible_tools,
                        replayed_tool_calls,
                        recorded_timer_calls: timer_calls,
                        suppressed_text_calls: total_text_calls,
                        suppressed_notification_calls: total_notification_calls,
                        skipped_yields: if matches!(summary.yield_kind, Some(ExecYieldKind::Manual))
                        {
                            pending.skipped_yields + 1
                        } else {
                            pending.skipped_yields
                        },
                        total_nested_tool_calls,
                    });
                } else {
                    session.pending_cell = None;
                }

                Ok(summary)
            }
            Err(err) => {
                let mut sessions = self.sessions.lock().await;
                if let Some(session) = sessions.get_mut(session_id) {
                    session.pending_cell = None;
                }
                Err(err)
            }
        }
    }

    async fn run_runtime_cell<F, Fut>(
        &self,
        cell_id: String,
        code: String,
        visible_tools: Vec<String>,
        stored_values: HashMap<String, serde_json::Value>,
        resume_state: runtime::ResumeState,
        invoke_tool: &mut F,
    ) -> Result<
        (
            ExecRunResult,
            HashMap<String, serde_json::Value>,
            runtime::RunCellMetadata,
        ),
        crate::tools::ToolError,
    >
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        let handle = tokio::runtime::Handle::current();
        let (request_tx, mut request_rx) =
            tokio::sync::mpsc::unbounded_channel::<ToolBridgeRequest>();
        let worker = tokio::task::spawn_blocking(move || {
            runtime::run_cell(
                handle,
                cell_id,
                code,
                visible_tools,
                stored_values,
                resume_state,
                move |tool_name: String, args_json: String| {
                    let (response_tx, response_rx) = std::sync::mpsc::sync_channel(1);
                    request_tx
                        .send(ToolBridgeRequest {
                            tool_name,
                            args_json,
                            response_tx,
                        })
                        .map_err(|_| {
                            crate::tools::ToolError::ExecutionFailed(
                                "Code mode nested tool bridge closed unexpectedly.".to_string(),
                            )
                        })?;
                    response_rx.recv().map_err(|_| {
                        crate::tools::ToolError::ExecutionFailed(
                            "Code mode nested tool response channel closed unexpectedly."
                                .to_string(),
                        )
                    })?
                },
            )
        });
        tokio::pin!(worker);

        loop {
            tokio::select! {
                biased;
                join_result = &mut worker => {
                    return join_result.map_err(|err| {
                        crate::tools::ToolError::ExecutionFailed(format!(
                            "Code mode runtime worker failed: {}",
                            err
                        ))
                    })?;
                }
                maybe_request = request_rx.recv() => {
                    match maybe_request {
                        Some(request) => {
                            let result = invoke_tool(request.tool_name, request.args_json).await;
                            let _ = request.response_tx.send(result);
                        }
                        None => {
                            return worker.await.map_err(|err| {
                                crate::tools::ToolError::ExecutionFailed(format!(
                                    "Code mode runtime worker failed: {}",
                                    err
                                ))
                            })?;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc as StdArc, Mutex as StdMutex};
    use std::time::Duration;

    #[tokio::test]
    async fn test_service_persists_values_across_cells() {
        let service = CodeModeService::default();
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        let first = service
            .execute(
                "session-a",
                r#"
store("answer", { value: 42 });
text("stored");
"#,
                Vec::new(),
                &mut invoke_tool,
            )
            .await
            .expect("first exec succeeds");
        assert!(first.output_text.contains("stored"));

        let second = service
            .execute(
                "session-a",
                r#"
const value = load("answer");
text(String(value.value));
"#,
                Vec::new(),
                &mut invoke_tool,
            )
            .await
            .expect("second exec succeeds");

        assert!(second.output_text.contains("42"));
    }

    #[tokio::test]
    async fn test_service_runs_nested_tool_calls() {
        let service = CodeModeService::default();
        let mut invoke_tool = |tool: String, args_json: String| async move {
            if tool == "echo_tool" {
                Ok(serde_json::json!({
                    "tool": tool,
                    "args": serde_json::from_str::<serde_json::Value>(&args_json).unwrap(),
                })
                .to_string())
            } else {
                Err(crate::tools::ToolError::ExecutionFailed(
                    "unexpected tool".to_string(),
                ))
            }
        };

        let result = service
            .execute(
                "session-b",
                r#"
const response = await tools.echo_tool({ value: "hello" });
text(response.args.value);
"#,
                vec!["echo_tool".to_string()],
                &mut invoke_tool,
            )
            .await
            .expect("exec succeeds");

        assert_eq!(result.nested_tool_calls, 1);
        assert!(result.output_text.contains("hello"));
    }

    #[tokio::test]
    async fn test_service_wait_resumes_yielded_cell_without_repeating_tool_calls() {
        let service = CodeModeService::default();
        let calls = StdArc::new(StdMutex::new(Vec::<String>::new()));
        let calls_for_tool = calls.clone();
        let mut invoke_tool = move |tool: String, args_json: String| {
            let calls_for_tool = calls_for_tool.clone();
            async move {
                calls_for_tool
                    .lock()
                    .unwrap()
                    .push(format!("{}:{}", tool, args_json));
                let value = serde_json::from_str::<serde_json::Value>(&args_json)
                    .unwrap()
                    .get("value")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                Ok(serde_json::json!({ "value": value }).to_string())
            }
        };

        let first = service
            .execute(
                "session-c",
                r#"
const first = await tools.echo_tool({ value: "hello" });
text(first.value);
yield_control("pause");
const second = await tools.echo_tool({ value: "world" });
text(second.value);
"#,
                vec!["echo_tool".to_string()],
                &mut invoke_tool,
            )
            .await
            .expect("first exec yields");

        assert!(first.yielded);
        assert_eq!(first.yield_kind, Some(ExecYieldKind::Manual));
        assert_eq!(first.yield_value, Some(serde_json::json!("pause")));
        assert_eq!(first.nested_tool_calls, 1);
        assert_eq!(first.output_text.trim(), "hello");

        let resumed = service
            .wait("session-c", Some(&first.cell_id), &mut invoke_tool)
            .await
            .expect("wait resumes pending cell");

        assert!(!resumed.yielded);
        assert_eq!(resumed.nested_tool_calls, 2);
        assert_eq!(resumed.output_text.trim(), "world");
        assert_eq!(
            calls.lock().unwrap().clone(),
            vec![
                "echo_tool:{\"value\":\"hello\"}".to_string(),
                "echo_tool:{\"value\":\"world\"}".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn test_service_wait_resumes_timer_driven_cells_with_incremental_output() {
        let service = CodeModeService::default();
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        let first = service
            .execute(
                "session-d",
                r#"
text("before");
setTimeout(async () => {
  text("later");
}, 20);
text("after");
"#,
                Vec::new(),
                &mut invoke_tool,
            )
            .await
            .expect("timer-based exec yields");

        assert!(first.yielded);
        assert_eq!(first.yield_kind, Some(ExecYieldKind::Timer));
        assert_eq!(first.output_text.trim(), "before\nafter");

        tokio::time::sleep(Duration::from_millis(30)).await;

        let resumed = service
            .wait("session-d", Some(&first.cell_id), &mut invoke_tool)
            .await
            .expect("timer-based wait resumes pending cell");

        assert!(!resumed.yielded);
        assert_eq!(resumed.output_text.trim(), "later");
    }
}
