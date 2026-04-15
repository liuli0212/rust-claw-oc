use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::cell::ActiveCellHandle;
use super::driver::CellDriver;
use super::protocol::DrainRequest;
use super::response::ExecRunResult;
use super::runtime;

#[derive(Debug, Default, Clone)]
pub struct CodeModeService {
    sessions: Arc<Mutex<HashMap<String, SessionState>>>,
}

type SharedCellDriver = Arc<Mutex<CellDriver>>;

#[derive(Debug, Default)]
struct SessionState {
    next_cell_seq: u64,
    stored_values: HashMap<String, runtime::value::StoredValue>,
    active_cell: Option<ActiveCellHandle>,
    live_driver: Option<SharedCellDriver>,
}

impl CodeModeService {
    /// Execute a new code-mode cell. Spawns a live JS runtime worker, performs
    /// an initial drain, and returns an `ExecRunResult` suitable for the LLM.
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
        let (cell_id, driver_handle) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions.entry(session_id.to_string()).or_default();

            if let Some(ref active_cell) = session.active_cell {
                if !active_cell.is_terminal() {
                    return Err(crate::tools::ToolError::ExecutionFailed(format!(
                        "Code mode cell `{}` is still active. Call `wait` until it completes.",
                        active_cell.cell_id
                    )));
                }
            }

            let cell_id = format!("cell-{}", session.next_cell_seq);
            session.next_cell_seq += 1;
            let driver_handle = Arc::new(Mutex::new(CellDriver::spawn_live(
                cell_id.clone(),
                code.to_string(),
                visible_tools,
                session.stored_values.clone(),
            )));
            session.active_cell = Some(ActiveCellHandle::new(cell_id.clone()));
            session.live_driver = Some(driver_handle.clone());
            (cell_id, driver_handle)
        };

        let batch = {
            let mut driver = driver_handle.lock().await;
            driver
                .drain_event_batch_with_request(DrainRequest::to_completion(), invoke_tool, false)
                .await
        };

        match batch {
            Ok(batch) => {
                self.apply_batch_to_session(session_id, &cell_id, batch)
                    .await
            }
            Err(err) => {
                self.abort_active_cell(session_id, &err.to_string()).await;
                Err(err)
            }
        }
    }

    /// Wait on an existing live code-mode cell. Drains new events from the
    /// live worker and returns an updated `ExecRunResult`.
    pub async fn wait_with_request<F, Fut>(
        &self,
        session_id: &str,
        requested_cell_id: Option<&str>,
        request: DrainRequest,
        invoke_tool: &mut F,
    ) -> Result<ExecRunResult, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        let (cell_id, driver_handle) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions.get_mut(session_id).ok_or_else(|| {
                crate::tools::ToolError::ExecutionFailed(
                    "No code-mode session found for this session.".to_string(),
                )
            })?;

            let active_cell = session.active_cell.as_ref().ok_or_else(|| {
                crate::tools::ToolError::ExecutionFailed(
                    "No active code-mode cell to wait on. Call `exec` first.".to_string(),
                )
            })?;

            if let Some(cell_id) = requested_cell_id {
                if cell_id != active_cell.cell_id {
                    return Err(crate::tools::ToolError::ExecutionFailed(format!(
                        "Cell ID mismatch: requested `{}` but active cell is `{}`.",
                        cell_id, active_cell.cell_id
                    )));
                }
            }

            if active_cell.is_terminal() {
                let summary = Self::build_exec_result_from_cell(active_cell);
                session.active_cell = None;
                session.live_driver = None;
                return Ok(summary);
            }

            let driver_handle = session.live_driver.clone().ok_or_else(|| {
                crate::tools::ToolError::ExecutionFailed(
                    "Active cell has no live driver. This is an internal error.".to_string(),
                )
            })?;

            (active_cell.cell_id.clone(), driver_handle)
        };

        let batch = {
            let mut driver = driver_handle.lock().await;
            driver
                .drain_event_batch_with_request(request, invoke_tool, true)
                .await
        };

        match batch {
            Ok(batch) => {
                self.apply_batch_to_session(session_id, &cell_id, batch)
                    .await
            }
            Err(err) => {
                self.abort_active_cell(session_id, &err.to_string()).await;
                Err(err)
            }
        }
    }

    pub async fn abort_active_cell(&self, session_id: &str, reason: &str) -> bool {
        let driver_handle = {
            let mut sessions = self.sessions.lock().await;
            let Some(session) = sessions.get_mut(session_id) else {
                return false;
            };
            session.active_cell = None;
            session.live_driver.take()
        };

        if let Some(driver_handle) = driver_handle {
            driver_handle.lock().await.request_cancel(reason);
            true
        } else {
            false
        }
    }

    async fn apply_batch_to_session(
        &self,
        session_id: &str,
        cell_id: &str,
        batch: crate::code_mode::driver::DriverDrainBatch,
    ) -> Result<ExecRunResult, crate::tools::ToolError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions.get_mut(session_id).ok_or_else(|| {
            crate::tools::ToolError::ExecutionFailed("Session disappeared during wait.".to_string())
        })?;

        let active_cell = session.active_cell.as_mut().ok_or_else(|| {
            crate::tools::ToolError::ExecutionFailed(format!(
                "Code mode cell `{}` was terminated before the drain completed.",
                cell_id
            ))
        })?;
        if active_cell.cell_id != cell_id {
            return Err(crate::tools::ToolError::ExecutionFailed(format!(
                "Code mode cell `{}` was superseded before the drain completed.",
                cell_id
            )));
        }
        active_cell.apply_drain_batch(&batch);

        if let Some(ref terminal) = batch.terminal_result {
            session.stored_values = terminal.1.clone();
        }

        let summary = Self::build_exec_result_from_cell(active_cell);
        if active_cell.is_terminal() {
            session.active_cell = None;
            session.live_driver = None;
        }

        Ok(summary)
    }

    /// Build an `ExecRunResult` from the current active cell state.
    fn build_exec_result_from_cell(cell: &ActiveCellHandle) -> ExecRunResult {
        let snapshot = cell.drain_snapshot();
        let render = snapshot.render_state();
        let nested_tool_calls = cell.nested_tool_call_count();
        let flushed = cell.is_yielding();

        ExecRunResult {
            cell_id: cell.cell_id.clone(),
            output_text: render.output_text,
            return_value: render.return_value,
            flush_value: render.flush_value,
            flushed,
            waiting_on_timer_ms: render.waiting_on_timer_ms,
            notifications: render.notifications,
            failure: render.failure,
            cancellation: render.cancellation,
            nested_tool_calls,
            truncated: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_rejects_when_no_active_cell_exists() {
        let service = CodeModeService::default();
        let mut invoke_tool =
            |_tool_name: String, _args_json: String| async move { Ok("null".to_string()) };

        let err = service
            .wait_with_request(
                "missing-session",
                None,
                DrainRequest::for_wait(Some(5), None),
                &mut invoke_tool,
            )
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("No code-mode session found"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn wait_rejects_cell_id_mismatch() {
        let service = CodeModeService::default();
        let mut invoke_tool =
            |_tool_name: String, _args_json: String| async move { Ok("null".to_string()) };

        let summary = service
            .execute(
                "session-a",
                "flush({ ok: true });",
                Vec::new(),
                &mut invoke_tool,
            )
            .await
            .expect("exec should yield");
        assert!(summary.flushed);

        let err = service
            .wait_with_request(
                "session-a",
                Some("cell-99"),
                DrainRequest::for_wait(Some(5), None),
                &mut invoke_tool,
            )
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("Cell ID mismatch"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn execute_rejects_new_cell_while_previous_one_is_active() {
        let service = CodeModeService::default();
        let mut invoke_tool =
            |_tool_name: String, _args_json: String| async move { Ok("null".to_string()) };

        let summary = service
            .execute(
                "session-b",
                r#"
                    setTimeout(() => {
                        text("done");
                    }, 1_000);
                "#,
                Vec::new(),
                &mut invoke_tool,
            )
            .await
            .expect("initial exec should yield on timer");
        assert!(summary.flushed);
        assert_eq!(summary.cell_id, "cell-0");

        let err = service
            .execute("session-b", "text('next');", Vec::new(), &mut invoke_tool)
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("still active"),
            "unexpected error: {err}"
        );
    }
}
