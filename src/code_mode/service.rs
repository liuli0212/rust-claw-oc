use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::sync::Mutex;
use tokio::sync::Notify;

use super::cell::{ActiveCellHandle, CellStatus};
use super::driver::{CellDriver, CellDriverControl};
use super::response::ExecRunResult;
use super::runtime;

#[derive(Debug, Default, Clone)]
pub struct CodeModeService {
    sessions: Arc<Mutex<HashMap<String, SessionState>>>,
}

type SharedCellDriver = Arc<Mutex<CellDriver>>;
type SharedCellHost = Arc<CellHostHandle>;

#[derive(Debug, Default)]
struct SessionState {
    next_cell_seq: u64,
    stored_values: HashMap<String, runtime::value::StoredValue>,
    active_cell: Option<ActiveCellHandle>,
    host_handle: Option<SharedCellHost>,
}

struct CellHostHandle {
    driver_handle: SharedCellDriver,
    driver_control: CellDriverControl,
    revision: AtomicU64,
    update_notify: Notify,
}

impl std::fmt::Debug for CellHostHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CellHostHandle")
            .field("revision", &self.revision.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl CellHostHandle {
    fn new(driver_handle: SharedCellDriver, driver_control: CellDriverControl) -> Self {
        Self {
            driver_handle,
            driver_control,
            revision: AtomicU64::new(0),
            update_notify: Notify::new(),
        }
    }

    fn current_revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }

    fn publish_update(&self) {
        self.revision.fetch_add(1, Ordering::SeqCst);
        self.update_notify.notify_waiters();
    }

    async fn wait_for_update_after(&self, revision: u64, timeout: Option<Duration>) -> bool {
        if self.current_revision() != revision {
            return true;
        }

        let notified = self.update_notify.notified();
        if self.current_revision() != revision {
            return true;
        }

        match timeout {
            Some(timeout) => tokio::time::timeout(timeout, notified).await.is_ok(),
            None => {
                notified.await;
                true
            }
        }
    }
}

impl CodeModeService {
    /// Execute a new code-mode cell. Spawns a live JS runtime worker, performs
    /// an initial background drain, and returns the first published
    /// `ExecRunResult` suitable for the LLM.
    pub async fn execute<F, Fut>(
        &self,
        session_id: &str,
        code: &str,
        visible_tools: Vec<String>,
        invoke_tool: F,
    ) -> Result<ExecRunResult, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut + Send + 'static,
        Fut: Future<Output = Result<String, crate::tools::ToolError>> + Send + 'static,
    {
        let (cell_id, host_handle) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions.entry(session_id.to_string()).or_default();

            if session
                .active_cell
                .as_ref()
                .is_some_and(ActiveCellHandle::is_terminal)
            {
                session.active_cell = None;
                session.host_handle = None;
            }

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
            let driver = CellDriver::spawn_live(
                cell_id.clone(),
                code.to_string(),
                visible_tools,
                session.stored_values.clone(),
            );
            let driver_control = driver.control_handle();
            let driver_handle = Arc::new(Mutex::new(driver));
            let host_handle = Arc::new(CellHostHandle::new(driver_handle, driver_control));
            session.active_cell = Some(ActiveCellHandle::new(cell_id.clone()));
            session.host_handle = Some(host_handle.clone());
            (cell_id, host_handle)
        };

        let (initial_summary_tx, initial_summary_rx) =
            oneshot::channel::<Result<ExecRunResult, crate::tools::ToolError>>();
        let service = self.clone();
        let session_id_owned = session_id.to_string();
        let cell_id_owned = cell_id.clone();
        let host_handle_for_task = host_handle.clone();

        tokio::spawn(async move {
            service
                .run_cell_host(
                    session_id_owned,
                    cell_id_owned,
                    host_handle_for_task,
                    invoke_tool,
                    initial_summary_tx,
                )
                .await;
        });

        match initial_summary_rx.await {
            Ok(result) => result,
            Err(_) => {
                self.abort_active_cell(
                    session_id,
                    "Code mode host task exited before publishing its initial state.",
                )
                .await;
                Err(crate::tools::ToolError::ExecutionFailed(
                    "Code mode host task exited before publishing its initial state.".to_string(),
                ))
            }
        }
    }

    /// Wait on an existing live code-mode cell. This observes the latest state
    /// published by the background host task and only resumes runtime execution
    /// when the cell is explicitly waiting on a JS timer.
    pub async fn wait_with_request(
        &self,
        session_id: &str,
        requested_cell_id: Option<&str>,
        wait_timeout_ms: Option<u64>,
    ) -> Result<ExecRunResult, crate::tools::ToolError> {
        let (cell_id, host_handle, revision) = {
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
                session.host_handle = None;
                return Ok(summary);
            }

            let host_handle = session.host_handle.clone().ok_or_else(|| {
                crate::tools::ToolError::ExecutionFailed(
                    "Active cell has no background host. This is an internal error.".to_string(),
                )
            })?;

            (
                active_cell.cell_id.clone(),
                host_handle.clone(),
                host_handle.current_revision(),
            )
        };

        let wait_timeout = wait_timeout_ms.map(Duration::from_millis);
        let updated = host_handle
            .wait_for_update_after(revision, wait_timeout)
            .await;
        if !updated {
            return self.read_cell_summary(session_id, &cell_id).await;
        }

        self.read_cell_summary(session_id, &cell_id).await
    }

    pub async fn abort_active_cell(&self, session_id: &str, reason: &str) -> bool {
        let host_handle = {
            let mut sessions = self.sessions.lock().await;
            let Some(session) = sessions.get_mut(session_id) else {
                return false;
            };
            session.active_cell = None;
            session.host_handle.take()
        };

        if let Some(host_handle) = host_handle {
            host_handle.driver_control.request_cancel(reason);
            true
        } else {
            false
        }
    }

    async fn run_cell_host<F, Fut>(
        self,
        session_id: String,
        cell_id: String,
        host_handle: SharedCellHost,
        mut invoke_tool: F,
        initial_summary_tx: oneshot::Sender<Result<ExecRunResult, crate::tools::ToolError>>,
    ) where
        F: FnMut(String, String) -> Fut + Send + 'static,
        Fut: Future<Output = Result<String, crate::tools::ToolError>> + Send + 'static,
    {
        let mut initial_summary_tx = Some(initial_summary_tx);

        loop {
            let batch = {
                let mut driver = host_handle.driver_handle.lock().await;
                driver
                    .drain_event_batch_with_request(&mut invoke_tool)
                    .await
            };

            match batch {
                Ok(batch) => match self
                    .publish_batch_to_session(&session_id, &cell_id, batch)
                    .await
                {
                    Ok((summary, disposition)) => {
                        host_handle.publish_update();

                        if let Some(tx) = initial_summary_tx.take() {
                            let _ = tx.send(Ok(summary.clone()));
                        }

                        match disposition {
                            CellDisposition::Continue => {}
                            CellDisposition::WaitingOnTimer => {}
                            CellDisposition::Terminal => return,
                        }
                    }
                    Err(err) => {
                        if let Some(tx) = initial_summary_tx.take() {
                            let _ = tx.send(Err(err));
                        }
                        return;
                    }
                },
                Err(err) => {
                    match self
                        .record_host_error_in_session(&session_id, &cell_id, &err)
                        .await
                    {
                        Some(summary) => {
                            host_handle.publish_update();
                            if let Some(tx) = initial_summary_tx.take() {
                                let _ = tx.send(Ok(summary));
                            }
                        }
                        None => {
                            if let Some(tx) = initial_summary_tx.take() {
                                let _ = tx.send(Err(err));
                            }
                        }
                    }
                    return;
                }
            }
        }
    }

    async fn publish_batch_to_session(
        &self,
        session_id: &str,
        cell_id: &str,
        batch: crate::code_mode::driver::DriverDrainBatch,
    ) -> Result<(ExecRunResult, CellDisposition), crate::tools::ToolError> {
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
        let disposition = if active_cell.is_terminal() {
            CellDisposition::Terminal
        } else if matches!(active_cell.status, CellStatus::WaitingOnJsTimer { .. }) {
            CellDisposition::WaitingOnTimer
        } else {
            CellDisposition::Continue
        };

        Ok((summary, disposition))
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

    async fn read_cell_summary(
        &self,
        session_id: &str,
        cell_id: &str,
    ) -> Result<ExecRunResult, crate::tools::ToolError> {
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
        if active_cell.cell_id != cell_id {
            return Err(crate::tools::ToolError::ExecutionFailed(format!(
                "Code mode cell `{}` was superseded before the wait completed.",
                cell_id
            )));
        }

        let summary = Self::build_exec_result_from_cell(active_cell);
        if active_cell.is_terminal() {
            session.active_cell = None;
            session.host_handle = None;
        }

        Ok(summary)
    }

    async fn record_host_error_in_session(
        &self,
        session_id: &str,
        cell_id: &str,
        err: &crate::tools::ToolError,
    ) -> Option<ExecRunResult> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions.get_mut(session_id)?;
        let active_cell = session.active_cell.as_mut()?;
        if active_cell.cell_id != cell_id {
            return None;
        }

        let snapshot = active_cell.drain_snapshot();
        let render = snapshot.render_state();
        let nested_tool_calls = active_cell.nested_tool_call_count();

        active_cell.status = CellStatus::Failed;
        active_cell.last_summary = Some(ExecRunResult {
            cell_id: active_cell.cell_id.clone(),
            output_text: render.output_text,
            return_value: render.return_value,
            flush_value: render.flush_value,
            flushed: false,
            waiting_on_timer_ms: None,
            notifications: render.notifications,
            failure: Some(err.to_string()),
            cancellation: None,
            nested_tool_calls,
            truncated: false,
        });

        Some(Self::build_exec_result_from_cell(active_cell))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellDisposition {
    Continue,
    WaitingOnTimer,
    Terminal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_rejects_when_no_active_cell_exists() {
        let service = CodeModeService::default();

        let err = service
            .wait_with_request("missing-session", None, Some(5))
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

        let summary = service
            .execute(
                "session-a",
                "flush({ ok: true });",
                Vec::new(),
                |_tool_name: String, _args_json: String| async move { Ok("null".to_string()) },
            )
            .await
            .expect("exec should yield");
        assert!(summary.flushed);

        let err = service
            .wait_with_request("session-a", Some("cell-99"), Some(5))
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

        let summary = service
            .execute(
                "session-b",
                r#"
                    setTimeout(() => {
                        text("done");
                    }, 1_000);
                "#,
                Vec::new(),
                |_tool_name: String, _args_json: String| async move { Ok("null".to_string()) },
            )
            .await
            .expect("initial exec should yield on timer");
        assert!(summary.flushed);
        assert_eq!(summary.cell_id, "cell-0");

        let err = service
            .execute(
                "session-b",
                "text('next');",
                Vec::new(),
                |_tool_name: String, _args_json: String| async move { Ok("null".to_string()) },
            )
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("still active"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn execute_cancels_infinite_loop() {
        let service = CodeModeService::default();
        let svc_clone = service.clone();

        let exec_result = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            svc_clone.execute(
                "session-loop",
                "while (true) {}",
                Vec::new(),
                |_tool_name: String, _args_json: String| async move { Ok("null".to_string()) },
            ),
        )
        .await;

        assert!(exec_result.is_err(), "execute should time out");

        let cancelled = service.abort_active_cell("session-loop", "Timeout").await;
        assert!(cancelled, "should successfully abort active cell");
    }

    #[tokio::test]
    async fn background_host_fulfills_nested_tool_calls_after_flush_without_wait() {
        let service = CodeModeService::default();

        let summary = service
            .execute(
                "session-c",
                r#"
                    flush({ stage: "starting" });
                    const response = await tools.echo_tool({ value: "done" });
                    text(response.value);
                "#,
                vec!["echo_tool".to_string()],
                |_tool_name: String, _args_json: String| async move {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    Ok(r#"{"value":"done"}"#.to_string())
                },
            )
            .await
            .expect("exec should publish initial flush state");

        assert!(
            summary.flushed,
            "initial summary should still reflect the flush boundary"
        );

        tokio::time::sleep(std::time::Duration::from_millis(75)).await;

        let final_summary = service
            .wait_with_request("session-c", Some(&summary.cell_id), Some(0))
            .await
            .expect("wait should observe the terminal summary");

        assert!(!final_summary.flushed);
        assert_eq!(final_summary.output_text, "done");
        assert_eq!(final_summary.return_value, None);
    }

    #[tokio::test]
    async fn wait_still_resumes_timer_boundaries() {
        let service = CodeModeService::default();

        let summary = service
            .execute(
                "session-d",
                r#"
                    setTimeout(() => {
                        text("timer done");
                    }, 20);
                "#,
                Vec::new(),
                |_tool_name: String, _args_json: String| async move { Ok("null".to_string()) },
            )
            .await
            .expect("exec should yield on timer");

        assert!(summary.flushed);
        assert!(summary.waiting_on_timer_ms.is_some());

        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        let final_summary = service
            .wait_with_request("session-d", Some(&summary.cell_id), Some(50))
            .await
            .expect("wait should resume the timer boundary");

        assert!(!final_summary.flushed);
        assert_eq!(final_summary.output_text, "timer done");
    }
}
