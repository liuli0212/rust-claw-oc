use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::cell::{ActiveCellHandle, CellDrainSnapshot, CellStatus};
use super::driver::{CellDriver, DriverCompletion, DriverDrainBatch};
use super::protocol::DrainRequest;
use super::response::ExecRunResult;

#[derive(Debug, Default, Clone)]
pub struct CodeModeService {
    sessions: Arc<Mutex<HashMap<String, SessionState>>>,
}

type SharedCellDriver = Arc<Mutex<CellDriver>>;

#[derive(Debug, Default)]
struct SessionState {
    next_cell_seq: u64,
    stored_values: HashMap<String, serde_json::Value>,
    active_cell: Option<ActiveCellHandle>,
    live_driver: Option<SharedCellDriver>,
}

#[derive(Debug, Clone)]
struct PendingCellContext {
    active_cell: ActiveCellHandle,
    drain_snapshot: CellDrainSnapshot,
    live_driver: SharedCellDriver,
}

#[derive(Debug)]
struct PendingDrainBatch {
    active_cell: ActiveCellHandle,
    prior_snapshot: CellDrainSnapshot,
    batch: DriverDrainBatch,
}

enum PendingDrainResolution {
    Progress {
        active_cell: ActiveCellHandle,
        prior_snapshot: CellDrainSnapshot,
        batch: Box<DriverDrainBatch>,
    },
    Completion {
        active_cell: ActiveCellHandle,
        completion: Box<DriverCompletion>,
    },
}

impl PendingDrainBatch {
    #[cfg(test)]
    #[allow(dead_code)]
    fn should_fallback_to_prior_snapshot(&self) -> bool {
        self.batch.requested_wait_for_event() && self.batch.is_empty()
    }

    fn into_resolution(self) -> Result<PendingDrainResolution, crate::tools::ToolError> {
        let PendingDrainBatch {
            active_cell,
            prior_snapshot,
            batch,
        } = self;

        if batch.terminal_result.is_some() {
            return Ok(PendingDrainResolution::Completion {
                active_cell,
                completion: Box::new(batch.into_completion()?),
            });
        }

        Ok(PendingDrainResolution::Progress {
            active_cell,
            prior_snapshot,
            batch: Box::new(batch),
        })
    }
}

struct RuntimeBatchInvocation {
    cell_id: String,
    code: String,
    visible_tools: Vec<String>,
    stored_values: HashMap<String, serde_json::Value>,
        request: DrainRequest,
}

impl RuntimeBatchInvocation {
    fn for_execute(
        cell_id: String,
        code: String,
        visible_tools: Vec<String>,
        stored_values: HashMap<String, serde_json::Value>,
    ) -> Self {
        Self::for_execute_with_request(
            cell_id,
            code,
            visible_tools,
            stored_values,
            DrainRequest::to_completion(),
        )
    }

    fn for_execute_with_request(
        cell_id: String,
        code: String,
        visible_tools: Vec<String>,
        stored_values: HashMap<String, serde_json::Value>,
        request: DrainRequest,
    ) -> Self {
        Self {
            cell_id,
            code,
            visible_tools,
            stored_values,
                        request,
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    fn for_pending_cell(
        active_cell: &ActiveCellHandle,
        stored_values: HashMap<String, serde_json::Value>,
        request: DrainRequest,
    ) -> Self {
        Self {
            cell_id: active_cell.cell_id.clone(),
            code: active_cell.code.clone(),
            visible_tools: active_cell.visible_tools.clone(),
            stored_values,
            request,
        }
    }
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
            if let Some(active_cell) = session.active_cell.as_ref() {
                return Err(crate::tools::ToolError::ExecutionFailed(format!(
                    "A pending code mode cell already exists for this session (`{}`). Use `wait` before starting a new `exec`.",
                    active_cell.cell_id
                )));
            }
            session.next_cell_seq += 1;
            (
                format!("cell_{}", session.next_cell_seq),
                session.stored_values.clone(),
            )
        };

        let completion = self
            .run_runtime_cell_batch_with_request(
                RuntimeBatchInvocation::for_execute(
                    cell_id.clone(),
                    code.to_string(),
                    visible_tools.clone(),
                    stored_values,
                ),
                invoke_tool,
            )
            .await?
            .into_completion()?;

        self.apply_execute_completion(
            session_id,
            cell_id,
            code.to_string(),
            visible_tools,
            completion,
        )
        .await
    }

    async fn pending_cell_context(
        &self,
        session_id: &str,
        requested_cell_id: Option<&str>,
    ) -> Result<PendingCellContext, crate::tools::ToolError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions.get_mut(session_id).ok_or_else(|| {
            crate::tools::ToolError::ExecutionFailed(
                "No pending code mode cell is available for this session.".to_string(),
            )
        })?;
        let active_cell = session.active_cell.clone().ok_or_else(|| {
            crate::tools::ToolError::ExecutionFailed(
                "No pending code mode cell is available for this session.".to_string(),
            )
        })?;

        if let Some(cell_id) = requested_cell_id {
            if cell_id != active_cell.cell_id {
                return Err(crate::tools::ToolError::ExecutionFailed(format!(
                    "Pending code mode cell mismatch: expected `{}`, got `{}`.",
                    active_cell.cell_id, cell_id
                )));
            }
        }

        Ok(PendingCellContext {
            drain_snapshot: active_cell.drain_snapshot(),
            live_driver: match session.live_driver.clone() {
                Some(live_driver) => live_driver,
                None => {
                    let live_driver = Arc::new(Mutex::new(CellDriver::spawn_live(
                        active_cell.cell_id.clone(),
                        active_cell.code.clone(),
                        active_cell.visible_tools.clone(),
                        session.stored_values.clone(),
                        matches!(active_cell.status, CellStatus::WaitingOnJsTimer { .. }),
                    )));
                    session.live_driver = Some(live_driver.clone());
                    live_driver
                }
            },
            active_cell,
        })
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
        self.wait_with_request(
            session_id,
            requested_cell_id,
            DrainRequest::wait_for_next_event(),
            invoke_tool,
        )
        .await
    }

    pub async fn poll<F, Fut>(
        &self,
        session_id: &str,
        requested_cell_id: Option<&str>,
        invoke_tool: &mut F,
    ) -> Result<ExecRunResult, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        self.wait_with_request(
            session_id,
            requested_cell_id,
            DrainRequest::poll_now(),
            invoke_tool,
        )
        .await
    }

    pub(crate) async fn wait_with_request<F, Fut>(
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
        let context = self
            .pending_cell_context(session_id, requested_cell_id)
            .await?;

        match self
            .drain_pending_cell_context(context, request, invoke_tool)
            .await
        {
            Ok(pending) => match pending.into_resolution() {
                Ok(resolution) => {
                    self.apply_pending_wait_resolution(session_id, resolution)
                        .await
                }
                Err(err) => {
                    self.clear_active_cell(session_id).await;
                    Err(err)
                }
            },
            Err(err) => {
                self.clear_active_cell(session_id).await;
                Err(err)
            }
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    async fn run_pending_cell_batch<F, Fut>(
        &self,
        session_id: &str,
        requested_cell_id: Option<&str>,
        request: DrainRequest,
        invoke_tool: &mut F,
    ) -> Result<PendingDrainBatch, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        let context = self
            .pending_cell_context(session_id, requested_cell_id)
            .await?;
        self.drain_pending_cell_context(context, request, invoke_tool)
            .await
    }

    async fn drain_pending_cell_context<F, Fut>(
        &self,
        context: PendingCellContext,
        request: DrainRequest,
        invoke_tool: &mut F,
    ) -> Result<PendingDrainBatch, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        let PendingCellContext {
            active_cell,
            drain_snapshot,
            live_driver,
        } = context;

        let batch = {
            let mut live_driver = live_driver.lock().await;
            live_driver
                .drain_event_batch_with_request(request, invoke_tool)
                .await?
        };

        Ok(PendingDrainBatch {
            active_cell,
            prior_snapshot: drain_snapshot,
            batch,
        })
    }

    async fn run_runtime_cell_batch_with_request<F, Fut>(
        &self,
        invocation: RuntimeBatchInvocation,
        invoke_tool: &mut F,
    ) -> Result<DriverDrainBatch, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        let RuntimeBatchInvocation {
            cell_id,
            code,
            visible_tools,
            stored_values,
            request,
        } = invocation;

        let mut driver =
            CellDriver::spawn(cell_id, code, visible_tools, stored_values);
        driver
            .drain_event_batch_with_request(request, invoke_tool)
            .await
    }

    async fn apply_terminal_wait_completion(
        &self,
        session_id: &str,
        active_cell: ActiveCellHandle,
        completion: DriverCompletion,
    ) -> Result<ExecRunResult, crate::tools::ToolError> {
        let DriverCompletion {
            runtime_result,
            events,
        } = completion;
        let last_event_seq =
            super::protocol::max_event_seq(&events).max(active_cell.last_event_seq);
        let (mut summary, stored_values) = runtime_result;
        let current_turn_nested_tool_calls = summary.nested_tool_calls;
        summary.nested_tool_calls =
            active_cell.drain_snapshot().nested_tool_calls + current_turn_nested_tool_calls;

        let next_active_cell = if summary.yielded {
            Some(active_cell.advance_with_yield(
                current_turn_nested_tool_calls,
                &summary,
                
                events,
                last_event_seq,
            ))
        } else {
            None
        };
        let live_driver = next_active_cell
            .as_ref()
            .map(|next_active_cell| self.spawn_live_driver(next_active_cell, &stored_values));

        self.persist_session_state(session_id, stored_values, next_active_cell, live_driver)
            .await;

        Ok(summary)
    }

    async fn apply_in_progress_wait_batch(
        &self,
        session_id: &str,
        active_cell: ActiveCellHandle,
        prior_snapshot: CellDrainSnapshot,
        batch: DriverDrainBatch,
    ) -> Result<ExecRunResult, crate::tools::ToolError> {
        let should_fallback_to_prior_snapshot =
            batch.requested_wait_for_event() && batch.is_empty();
        let DriverDrainBatch {
            request: _,
            terminal_result: _,
            events,
            
        } = batch;
        let last_event_seq =
            super::protocol::max_event_seq(&events).max(active_cell.last_event_seq);
        let active_cell = active_cell.advance_with_events(events, last_event_seq);
        let result = if should_fallback_to_prior_snapshot {
            prior_snapshot.to_exec_result(active_cell.cell_id.clone())
        } else {
            active_cell
                .drain_snapshot()
                .to_exec_result(active_cell.cell_id.clone())
        };
        self.persist_active_cell(session_id, Some(active_cell))
            .await;
        Ok(result)
    }

    async fn apply_pending_wait_resolution(
        &self,
        session_id: &str,
        resolution: PendingDrainResolution,
    ) -> Result<ExecRunResult, crate::tools::ToolError> {
        match resolution {
            PendingDrainResolution::Progress {
                active_cell,
                prior_snapshot,
                batch,
            } => {
                self.apply_in_progress_wait_batch(session_id, active_cell, prior_snapshot, *batch)
                    .await
            }
            PendingDrainResolution::Completion {
                active_cell,
                completion,
            } => {
                self.apply_terminal_wait_completion(session_id, active_cell, *completion)
                    .await
            }
        }
    }

    async fn apply_execute_completion(
        &self,
        session_id: &str,
        cell_id: String,
        code: String,
        visible_tools: Vec<String>,
        completion: DriverCompletion,
    ) -> Result<ExecRunResult, crate::tools::ToolError> {
        let DriverCompletion {
            runtime_result,
            events,
        } = completion;
        let last_event_seq = super::protocol::max_event_seq(&events);
        let (mut summary, stored_values) = runtime_result;
        let next_active_cell = if summary.yielded {
            Some(ActiveCellHandle::from_initial_yield(
                cell_id.clone(),
                code,
                visible_tools,
                &summary,
                
                events,
                last_event_seq,
            ))
        } else {
            None
        };
        let live_driver = next_active_cell
            .as_ref()
            .map(|next_active_cell| self.spawn_live_driver(next_active_cell, &stored_values));
        self.persist_session_state(session_id, stored_values, next_active_cell, live_driver)
            .await;
        summary.cell_id = cell_id;
        Ok(summary)
    }

    fn spawn_live_driver(
        &self,
        active_cell: &ActiveCellHandle,
        stored_values: &HashMap<String, serde_json::Value>,
    ) -> SharedCellDriver {
        Arc::new(Mutex::new(CellDriver::spawn_live(
            active_cell.cell_id.clone(),
            active_cell.code.clone(),
            active_cell.visible_tools.clone(),
            stored_values.clone(),
            matches!(active_cell.status, CellStatus::WaitingOnJsTimer { .. }),
        )))
    }

    async fn persist_session_state(
        &self,
        session_id: &str,
        stored_values: HashMap<String, serde_json::Value>,
        active_cell: Option<ActiveCellHandle>,
        live_driver: Option<SharedCellDriver>,
    ) {
        let mut sessions = self.sessions.lock().await;
        let session = sessions.entry(session_id.to_string()).or_default();
        session.stored_values = stored_values;
        session.active_cell = active_cell;
        session.live_driver = live_driver;
    }

    async fn persist_active_cell(&self, session_id: &str, active_cell: Option<ActiveCellHandle>) {
        let mut sessions = self.sessions.lock().await;
        let session = sessions.entry(session_id.to_string()).or_default();
        session.active_cell = active_cell;
    }

    async fn clear_active_cell(&self, session_id: &str) {
        let mut sessions = self.sessions.lock().await;
        let session = sessions.entry(session_id.to_string()).or_default();
        session.active_cell = None;
        session.live_driver = None;
    }
}

