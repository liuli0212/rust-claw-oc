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
        let (cell_id, stored_values) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions.entry(session_id.to_string()).or_default();

            // Reject if a nonterminal cell already exists
            if let Some(ref active_cell) = session.active_cell {
                if !active_cell.is_terminal() {
                    return Err(crate::tools::ToolError::ExecutionFailed(format!(
                        "Code mode cell `{}` is still active. Call `wait` or cancel it first.",
                        active_cell.cell_id
                    )));
                }
            }

            let cell_id = format!("cell-{}", session.next_cell_seq);
            session.next_cell_seq += 1;
            (cell_id, session.stored_values.clone())
        };

        let mut driver = CellDriver::spawn_live(
            cell_id.clone(),
            code.to_string(),
            visible_tools,
            stored_values,
            false,
        );

        // Drain to completion — this also fulfills nested tool calls via invoke_tool
        let batch = driver
            .drain_event_batch_with_request(DrainRequest::to_completion(), invoke_tool, false)
            .await?;

        // Update session state
        let mut sessions = self.sessions.lock().await;
        let session = sessions.entry(session_id.to_string()).or_default();

        let mut active_cell = ActiveCellHandle::new(cell_id);
        active_cell.apply_drain_batch(&batch);

        // If the batch is terminal, update stored_values from the runtime result
        if let Some(ref terminal) = batch.terminal_result {
            session.stored_values = terminal.1.clone();
        }

        let summary = Self::build_exec_result_from_cell(&active_cell);

        if active_cell.is_terminal() {
            session.active_cell = None;
            session.live_driver = None;
        } else {
            session.active_cell = Some(active_cell);
            session.live_driver = Some(Arc::new(Mutex::new(driver)));
        }

        Ok(summary)
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
        let driver_handle = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions.get_mut(session_id).ok_or_else(|| {
                crate::tools::ToolError::ExecutionFailed(
                    "No code-mode session found for this session.".to_string(),
                )
            })?;

            // Validate that we have an active cell
            let active_cell = session.active_cell.as_ref().ok_or_else(|| {
                crate::tools::ToolError::ExecutionFailed(
                    "No active code-mode cell to wait on. Call `exec` first.".to_string(),
                )
            })?;

            // Validate cell_id if provided
            if let Some(cell_id) = requested_cell_id {
                if cell_id != active_cell.cell_id {
                    return Err(crate::tools::ToolError::ExecutionFailed(format!(
                        "Cell ID mismatch: requested `{}` but active cell is `{}`.",
                        cell_id, active_cell.cell_id
                    )));
                }
            }

            // If already terminal, return the sticky terminal snapshot
            if active_cell.is_terminal() {
                let summary = Self::build_exec_result_from_cell(active_cell);
                session.active_cell = None;
                session.live_driver = None;
                return Ok(summary);
            }

            session.live_driver.clone().ok_or_else(|| {
                crate::tools::ToolError::ExecutionFailed(
                    "Active cell has no live driver. This is an internal error.".to_string(),
                )
            })?
        };

        // Drain outside the session lock
        let batch = {
            let mut driver = driver_handle.lock().await;
            driver
                .drain_event_batch_with_request(request, invoke_tool, true)
                .await?
        };

        // Re-acquire the lock to update session state
        let mut sessions = self.sessions.lock().await;
        let session = sessions.get_mut(session_id).ok_or_else(|| {
            crate::tools::ToolError::ExecutionFailed("Session disappeared during wait.".to_string())
        })?;

        if let Some(ref mut active_cell) = session.active_cell {
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
        } else {
            Err(crate::tools::ToolError::ExecutionFailed(
                "Active cell was cleared during wait.".to_string(),
            ))
        }
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
            notifications: render.notifications,
            failure: render.failure,
            cancellation: render.cancellation,
            nested_tool_calls,
            truncated: false,
        }
    }
}
