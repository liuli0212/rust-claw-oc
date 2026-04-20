use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::oneshot;
use tokio::sync::Mutex;
use tokio::sync::Notify;

use super::cell::{ActiveCellHandle, CellStatus};
use super::driver::{CellDriver, CellDriverControl, DriverUpdate, DriverUpdateBatch};
use super::response::{ExecLifecycle, ExecProgressKind, ExecRunResult};
use super::runtime;
use crate::trace::TraceStatus;

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

#[derive(Debug, Clone)]
struct PublicationTracker {
    auto_flush_interval: Option<Duration>,
    last_published_at: Instant,
    last_published_progress_seq: u64,
    latest_progress_seq: u64,
}

impl PublicationTracker {
    fn new(auto_flush_ms: Option<u64>) -> Self {
        Self {
            auto_flush_interval: auto_flush_ms.map(Duration::from_millis),
            last_published_at: Instant::now(),
            last_published_progress_seq: 0,
            latest_progress_seq: 0,
        }
    }

    fn observe_batch(&mut self, metadata: &BatchPublicationMetadata) {
        if let Some(seq) = metadata.latest_progress_seq {
            self.latest_progress_seq = self.latest_progress_seq.max(seq);
        }
    }

    fn has_unpublished_progress(&self) -> bool {
        self.latest_progress_seq > self.last_published_progress_seq
    }

    fn should_auto_flush_now(&self) -> bool {
        let Some(interval) = self.auto_flush_interval else {
            return false;
        };

        self.has_unpublished_progress() && self.last_published_at.elapsed() >= interval
    }

    fn next_idle_timeout(&self) -> Option<Duration> {
        let interval = self.auto_flush_interval?;
        if !self.has_unpublished_progress() {
            return None;
        }

        Some(interval.saturating_sub(self.last_published_at.elapsed()))
    }

    fn mark_published(&mut self) {
        self.last_published_progress_seq = self.latest_progress_seq;
        self.last_published_at = Instant::now();
    }
}

#[derive(Debug, Clone, Default)]
struct BatchPublicationMetadata {
    explicit_flush_value: Option<Option<Value>>,
    latest_progress_seq: Option<u64>,
}

impl BatchPublicationMetadata {
    fn from_batch(batch: &DriverUpdateBatch) -> Self {
        let mut metadata = Self::default();

        for event in &batch.events {
            match event {
                crate::code_mode::protocol::RuntimeEvent::Text { seq, .. }
                | crate::code_mode::protocol::RuntimeEvent::Notification { seq, .. } => {
                    metadata.latest_progress_seq =
                        Some(metadata.latest_progress_seq.unwrap_or_default().max(*seq));
                }
                crate::code_mode::protocol::RuntimeEvent::Flush { seq, value } => {
                    metadata.explicit_flush_value = Some(value.clone());
                    metadata.latest_progress_seq =
                        Some(metadata.latest_progress_seq.unwrap_or_default().max(*seq));
                }
                _ => {}
            }
        }

        metadata
    }
}

impl CodeModeService {
    /// Execute a new code-mode cell. Spawns a live JS runtime worker, performs
    /// an initial background update, and returns the first published
    /// `ExecRunResult` suitable for the LLM.
    pub async fn execute<F, Fut>(
        &self,
        session_id: &str,
        code: &str,
        auto_flush_ms: Option<u64>,
        visible_tools: Vec<String>,
        invoke_tool: F,
        cell_span: Option<crate::trace::TraceSpanHandle>,
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
                    auto_flush_ms,
                    invoke_tool,
                    initial_summary_tx,
                    cell_span,
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

    /// Wait on an existing live code-mode cell. This observes the latest
    /// progress publication from the background host task and falls back to the
    /// current snapshot when the optional timeout elapses.
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
                let summary = Self::build_exec_result_from_cell(active_cell, None, None);
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
            return self.read_cell_summary(session_id, &cell_id, false).await;
        }

        self.read_cell_summary(session_id, &cell_id, true).await
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
        auto_flush_ms: Option<u64>,
        mut invoke_tool: F,
        initial_summary_tx: oneshot::Sender<Result<ExecRunResult, crate::tools::ToolError>>,
        mut cell_span: Option<crate::trace::TraceSpanHandle>,
    ) where
        F: FnMut(String, String) -> Fut + Send + 'static,
        Fut: Future<Output = Result<String, crate::tools::ToolError>> + Send + 'static,
    {
        let mut initial_summary_tx = Some(initial_summary_tx);
        let mut publication_tracker = PublicationTracker::new(auto_flush_ms);
        let mut final_trace_status = TraceStatus::Ok;
        let mut final_trace_summary: Option<String> = None;
        let mut final_trace_attrs = serde_json::json!({
            "cell_id": cell_id.clone(),
        });

        loop {
            let driver_update = {
                let mut driver = host_handle.driver_handle.lock().await;
                driver
                    .next_update(publication_tracker.next_idle_timeout())
                    .await
            };

            match driver_update {
                Ok(DriverUpdate::Batch(batch)) => {
                    let batch_metadata = BatchPublicationMetadata::from_batch(&batch);
                    publication_tracker.observe_batch(&batch_metadata);

                    match self
                        .update_session_from_batch(&session_id, &cell_id, batch)
                        .await
                    {
                        Ok(disposition) => {
                            let publication = if disposition == CellDisposition::Terminal {
                                match self
                                    .peek_cell_summary(&session_id, &cell_id, None, None)
                                    .await
                                {
                                    Ok(summary) => Some(summary),
                                    Err(err) => {
                                        final_trace_status =
                                            Self::trace_status_from_host_error(&err);
                                        final_trace_summary = Some(err.to_string());
                                        final_trace_attrs = serde_json::json!({
                                            "cell_id": cell_id.clone(),
                                            "error": err.to_string(),
                                        });
                                        if let Some(tx) = initial_summary_tx.take() {
                                            let _ = tx.send(Err(err));
                                        }
                                        break;
                                    }
                                }
                            } else if let Some(flush_value) =
                                batch_metadata.explicit_flush_value.clone()
                            {
                                match self
                                    .peek_cell_summary(
                                        &session_id,
                                        &cell_id,
                                        Some(ExecProgressKind::ExplicitFlush),
                                        flush_value,
                                    )
                                    .await
                                {
                                    Ok(summary) => {
                                        publication_tracker.mark_published();
                                        Some(summary)
                                    }
                                    Err(err) => {
                                        final_trace_status =
                                            Self::trace_status_from_host_error(&err);
                                        final_trace_summary = Some(err.to_string());
                                        final_trace_attrs = serde_json::json!({
                                            "cell_id": cell_id.clone(),
                                            "error": err.to_string(),
                                        });
                                        if let Some(tx) = initial_summary_tx.take() {
                                            let _ = tx.send(Err(err));
                                        }
                                        break;
                                    }
                                }
                            } else if publication_tracker.should_auto_flush_now() {
                                match self
                                    .peek_cell_summary(
                                        &session_id,
                                        &cell_id,
                                        Some(ExecProgressKind::AutoFlush),
                                        None,
                                    )
                                    .await
                                {
                                    Ok(summary) => {
                                        publication_tracker.mark_published();
                                        Some(summary)
                                    }
                                    Err(err) => {
                                        final_trace_status =
                                            Self::trace_status_from_host_error(&err);
                                        final_trace_summary = Some(err.to_string());
                                        final_trace_attrs = serde_json::json!({
                                            "cell_id": cell_id.clone(),
                                            "error": err.to_string(),
                                        });
                                        if let Some(tx) = initial_summary_tx.take() {
                                            let _ = tx.send(Err(err));
                                        }
                                        break;
                                    }
                                }
                            } else {
                                None
                            };

                            if let Some(summary) = publication {
                                if let Err(err) = self
                                    .publish_summary_update(
                                        &session_id,
                                        &cell_id,
                                        &host_handle,
                                        &summary,
                                    )
                                    .await
                                {
                                    final_trace_status = Self::trace_status_from_host_error(&err);
                                    final_trace_summary = Some(err.to_string());
                                    final_trace_attrs = serde_json::json!({
                                        "cell_id": cell_id.clone(),
                                        "error": err.to_string(),
                                    });
                                    if let Some(tx) = initial_summary_tx.take() {
                                        let _ = tx.send(Err(err));
                                    }
                                    break;
                                }
                                if let Some(tx) = initial_summary_tx.take() {
                                    let _ = tx.send(Ok(summary.clone()));
                                }
                                if disposition == CellDisposition::Terminal {
                                    final_trace_status = Self::trace_status_from_summary(&summary);
                                    final_trace_summary = Self::trace_summary_from_result(&summary);
                                    final_trace_attrs = serde_json::json!({
                                        "cell_id": cell_id.clone(),
                                        "lifecycle": summary.lifecycle.clone(),
                                        "nested_tool_calls": summary.nested_tool_calls,
                                    });
                                }
                            }

                            if disposition == CellDisposition::Terminal {
                                break;
                            }
                        }
                        Err(err) => {
                            final_trace_status = Self::trace_status_from_host_error(&err);
                            final_trace_summary = Some(err.to_string());
                            final_trace_attrs = serde_json::json!({
                                "cell_id": cell_id.clone(),
                                "error": err.to_string(),
                            });
                            if let Some(tx) = initial_summary_tx.take() {
                                let _ = tx.send(Err(err));
                            }
                            break;
                        }
                    }
                }
                Ok(DriverUpdate::PendingTool { batch, request }) => {
                    match self
                        .update_session_from_batch(&session_id, &cell_id, batch)
                        .await
                    {
                        Ok(CellDisposition::Terminal) => {
                            let err = crate::tools::ToolError::ExecutionFailed(
                                "Code mode entered a terminal state while dispatching a nested tool."
                                    .to_string(),
                            );
                            final_trace_status = TraceStatus::Error;
                            final_trace_summary = Some(err.to_string());
                            final_trace_attrs = serde_json::json!({
                                "cell_id": cell_id.clone(),
                                "error": err.to_string(),
                            });
                            if let Some(tx) = initial_summary_tx.take() {
                                let _ = tx.send(Err(err));
                            }
                            break;
                        }
                        Ok(CellDisposition::Continue) => {
                            let current_summary = match self
                                .peek_cell_summary(&session_id, &cell_id, None, None)
                                .await
                            {
                                Ok(summary) => summary,
                                Err(err) => {
                                    final_trace_status = Self::trace_status_from_host_error(&err);
                                    final_trace_summary = Some(err.to_string());
                                    final_trace_attrs = serde_json::json!({
                                        "cell_id": cell_id.clone(),
                                        "error": err.to_string(),
                                    });
                                    if let Some(tx) = initial_summary_tx.take() {
                                        let _ = tx.send(Err(err));
                                    }
                                    break;
                                }
                            };

                            if let Err(err) = self
                                .publish_summary_update(
                                    &session_id,
                                    &cell_id,
                                    &host_handle,
                                    &current_summary,
                                )
                                .await
                            {
                                final_trace_status = Self::trace_status_from_host_error(&err);
                                final_trace_summary = Some(err.to_string());
                                final_trace_attrs = serde_json::json!({
                                    "cell_id": cell_id.clone(),
                                    "error": err.to_string(),
                                });
                                if let Some(tx) = initial_summary_tx.take() {
                                    let _ = tx.send(Err(err));
                                }
                                break;
                            }

                            let tool_result = if initial_summary_tx.is_some() {
                                let tool_name = request.tool_name.clone();
                                let args_json = request.args_json.clone();
                                let mut invoke_future =
                                    std::pin::pin!(invoke_tool(tool_name, args_json));
                                let publish_after = Duration::from_millis(25);

                                tokio::select! {
                                    result = &mut invoke_future => result,
                                    _ = tokio::time::sleep(publish_after) => {
                                        if let Some(tx) = initial_summary_tx.take() {
                                            let _ = tx.send(Ok(current_summary.clone()));
                                        }
                                        invoke_future.await
                                    }
                                }
                            } else {
                                invoke_tool(request.tool_name.clone(), request.args_json.clone())
                                    .await
                            };
                            let completion_result = {
                                let mut driver = host_handle.driver_handle.lock().await;
                                driver.complete_pending_tool_call(&request, tool_result)
                            };
                            if let Err(err) = completion_result {
                                final_trace_status = Self::trace_status_from_host_error(&err);
                                final_trace_summary = Some(err.to_string());
                                final_trace_attrs = serde_json::json!({
                                    "cell_id": cell_id.clone(),
                                    "request_id": request.request_id.clone(),
                                    "error": err.to_string(),
                                });
                                match self
                                    .record_host_error_in_session(&session_id, &cell_id, &err)
                                    .await
                                {
                                    Some(summary) => {
                                        let _ = self
                                            .publish_summary_update(
                                                &session_id,
                                                &cell_id,
                                                &host_handle,
                                                &summary,
                                            )
                                            .await;
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
                                break;
                            }
                        }
                        Err(err) => {
                            final_trace_status = Self::trace_status_from_host_error(&err);
                            final_trace_summary = Some(err.to_string());
                            final_trace_attrs = serde_json::json!({
                                "cell_id": cell_id.clone(),
                                "request_id": request.request_id.clone(),
                                "error": err.to_string(),
                            });
                            if let Some(tx) = initial_summary_tx.take() {
                                let _ = tx.send(Err(err));
                            }
                            break;
                        }
                    }
                }
                Ok(DriverUpdate::Idle) => {
                    if !publication_tracker.should_auto_flush_now() {
                        continue;
                    }

                    match self
                        .peek_cell_summary(
                            &session_id,
                            &cell_id,
                            Some(ExecProgressKind::AutoFlush),
                            None,
                        )
                        .await
                    {
                        Ok(summary) => {
                            publication_tracker.mark_published();
                            if let Err(err) = self
                                .publish_summary_update(
                                    &session_id,
                                    &cell_id,
                                    &host_handle,
                                    &summary,
                                )
                                .await
                            {
                                final_trace_status = Self::trace_status_from_host_error(&err);
                                final_trace_summary = Some(err.to_string());
                                final_trace_attrs = serde_json::json!({
                                    "cell_id": cell_id.clone(),
                                    "error": err.to_string(),
                                });
                                if let Some(tx) = initial_summary_tx.take() {
                                    let _ = tx.send(Err(err));
                                }
                                break;
                            }
                            if let Some(tx) = initial_summary_tx.take() {
                                let _ = tx.send(Ok(summary));
                            }
                        }
                        Err(err) => {
                            final_trace_status = Self::trace_status_from_host_error(&err);
                            final_trace_summary = Some(err.to_string());
                            final_trace_attrs = serde_json::json!({
                                "cell_id": cell_id.clone(),
                                "error": err.to_string(),
                            });
                            if let Some(tx) = initial_summary_tx.take() {
                                let _ = tx.send(Err(err));
                            }
                            break;
                        }
                    }
                }
                Err(err) => {
                    final_trace_status = Self::trace_status_from_host_error(&err);
                    final_trace_summary = Some(err.to_string());
                    final_trace_attrs = serde_json::json!({
                        "cell_id": cell_id.clone(),
                        "error": err.to_string(),
                    });
                    match self
                        .record_host_error_in_session(&session_id, &cell_id, &err)
                        .await
                    {
                        Some(summary) => {
                            let _ = self
                                .publish_summary_update(
                                    &session_id,
                                    &cell_id,
                                    &host_handle,
                                    &summary,
                                )
                                .await;
                            final_trace_status = Self::trace_status_from_summary(&summary);
                            final_trace_summary = Self::trace_summary_from_result(&summary);
                            final_trace_attrs = serde_json::json!({
                                "cell_id": cell_id.clone(),
                                "lifecycle": summary.lifecycle.clone(),
                                "nested_tool_calls": summary.nested_tool_calls,
                                "failure": summary.failure.clone(),
                                "cancellation": summary.cancellation.clone(),
                            });
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
                    break;
                }
            }
        }

        if let Some(span) = cell_span.take() {
            span.finish(
                "code_mode_cell_finished",
                final_trace_status,
                final_trace_summary,
                final_trace_attrs,
            );
        }
    }

    async fn update_session_from_batch(
        &self,
        session_id: &str,
        cell_id: &str,
        batch: DriverUpdateBatch,
    ) -> Result<CellDisposition, crate::tools::ToolError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions.get_mut(session_id).ok_or_else(|| {
            crate::tools::ToolError::ExecutionFailed("Session disappeared during wait.".to_string())
        })?;

        let active_cell = session.active_cell.as_mut().ok_or_else(|| {
            crate::tools::ToolError::ExecutionFailed(format!(
                "Code mode cell `{}` was terminated before the background state update completed.",
                cell_id
            ))
        })?;
        if active_cell.cell_id != cell_id {
            return Err(crate::tools::ToolError::ExecutionFailed(format!(
                "Code mode cell `{}` was superseded before the background state update completed.",
                cell_id
            )));
        }
        active_cell.update_from_batch(&batch);

        if let Some(ref terminal) = batch.terminal_result {
            session.stored_values = terminal.1.clone();
        }

        if active_cell.is_terminal() {
            Ok(CellDisposition::Terminal)
        } else {
            Ok(CellDisposition::Continue)
        }
    }

    fn build_exec_result_from_cell(
        cell: &ActiveCellHandle,
        progress_kind: Option<ExecProgressKind>,
        flush_value: Option<Value>,
    ) -> ExecRunResult {
        let snapshot = cell.state_snapshot();
        let render = snapshot.build_render_state();
        let nested_tool_calls = cell.nested_tool_call_count();
        let lifecycle = match cell.status {
            CellStatus::Completed => ExecLifecycle::Completed,
            CellStatus::Failed => ExecLifecycle::Failed,
            CellStatus::Cancelled => ExecLifecycle::Cancelled,
            _ => ExecLifecycle::Running,
        };
        let waiting_on_timer_ms = match cell.status {
            CellStatus::WaitingOnJsTimer { next_due_in_ms } => next_due_in_ms,
            _ => None,
        };
        let waiting_on_tool_request_id = match &cell.status {
            CellStatus::WaitingOnTool { request_id } => Some(request_id.clone()),
            _ => None,
        };

        ExecRunResult {
            cell_id: cell.cell_id.clone(),
            output_text: render.output_text,
            return_value: render.return_value,
            flush_value: if progress_kind == Some(ExecProgressKind::ExplicitFlush) {
                flush_value
            } else {
                None
            },
            lifecycle,
            progress_kind: progress_kind.clone(),
            flushed: progress_kind.is_some(),
            waiting_on_tool_request_id,
            waiting_on_timer_ms,
            notifications: render.notifications,
            failure: render.failure,
            cancellation: render.cancellation,
            nested_tool_calls,
            truncated: false,
        }
    }

    async fn peek_cell_summary(
        &self,
        session_id: &str,
        cell_id: &str,
        progress_kind: Option<ExecProgressKind>,
        flush_value: Option<Value>,
    ) -> Result<ExecRunResult, crate::tools::ToolError> {
        let sessions = self.sessions.lock().await;
        let session = sessions.get(session_id).ok_or_else(|| {
            crate::tools::ToolError::ExecutionFailed(
                "No code-mode session found for this session.".to_string(),
            )
        })?;
        let active_cell = session.active_cell.as_ref().ok_or_else(|| {
            crate::tools::ToolError::ExecutionFailed(
                "No active code-mode cell to inspect. Call `exec` first.".to_string(),
            )
        })?;
        if active_cell.cell_id != cell_id {
            return Err(crate::tools::ToolError::ExecutionFailed(format!(
                "Code mode cell `{}` was superseded before the summary was published.",
                cell_id
            )));
        }

        Ok(Self::build_exec_result_from_cell(
            active_cell,
            progress_kind,
            flush_value,
        ))
    }

    async fn read_cell_summary(
        &self,
        session_id: &str,
        cell_id: &str,
        prefer_publication: bool,
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

        let summary = if prefer_publication {
            active_cell
                .last_publication
                .clone()
                .unwrap_or_else(|| Self::build_exec_result_from_cell(active_cell, None, None))
        } else {
            Self::build_exec_result_from_cell(active_cell, None, None)
        };
        if active_cell.is_terminal() {
            session.active_cell = None;
            session.host_handle = None;
        }

        Ok(summary)
    }

    async fn store_publication_summary(
        &self,
        session_id: &str,
        cell_id: &str,
        summary: &ExecRunResult,
    ) -> Result<(), crate::tools::ToolError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions.get_mut(session_id).ok_or_else(|| {
            crate::tools::ToolError::ExecutionFailed(
                "No code-mode session found for this session.".to_string(),
            )
        })?;
        let active_cell = session.active_cell.as_mut().ok_or_else(|| {
            crate::tools::ToolError::ExecutionFailed(
                "No active code-mode cell to store progress for.".to_string(),
            )
        })?;
        if active_cell.cell_id != cell_id {
            return Err(crate::tools::ToolError::ExecutionFailed(format!(
                "Code mode cell `{}` was superseded before the publication summary was stored.",
                cell_id
            )));
        }

        active_cell.last_publication = Some(summary.clone());
        Ok(())
    }

    async fn publish_summary_update(
        &self,
        session_id: &str,
        cell_id: &str,
        host_handle: &SharedCellHost,
        summary: &ExecRunResult,
    ) -> Result<(), crate::tools::ToolError> {
        self.store_publication_summary(session_id, cell_id, summary)
            .await?;
        host_handle.publish_update();
        Ok(())
    }

    fn trace_status_from_summary(summary: &ExecRunResult) -> TraceStatus {
        match summary.lifecycle {
            ExecLifecycle::Completed => TraceStatus::Ok,
            ExecLifecycle::Failed => TraceStatus::Error,
            ExecLifecycle::Cancelled => TraceStatus::Cancelled,
            ExecLifecycle::Running => TraceStatus::Ok,
        }
    }

    fn trace_summary_from_result(summary: &ExecRunResult) -> Option<String> {
        match summary.lifecycle {
            ExecLifecycle::Completed => Some("completed".to_string()),
            ExecLifecycle::Failed => summary.failure.clone().or(Some("failed".to_string())),
            ExecLifecycle::Cancelled => summary
                .cancellation
                .clone()
                .or(Some("cancelled".to_string())),
            ExecLifecycle::Running => None,
        }
    }

    fn trace_status_from_host_error(err: &crate::tools::ToolError) -> TraceStatus {
        if matches!(err, crate::tools::ToolError::Timeout) {
            TraceStatus::TimedOut
        } else {
            let msg = err.to_string().to_lowercase();
            if msg.contains("cancel")
                || msg.contains("interrupted")
                || msg.contains("terminated before the background state update completed")
            {
                TraceStatus::Cancelled
            } else {
                TraceStatus::Error
            }
        }
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

        let snapshot = active_cell.state_snapshot();
        let render = snapshot.build_render_state();
        let nested_tool_calls = active_cell.nested_tool_call_count();

        active_cell.status = CellStatus::Failed;
        active_cell.last_summary = Some(ExecRunResult {
            cell_id: active_cell.cell_id.clone(),
            output_text: render.output_text,
            return_value: render.return_value,
            flush_value: None,
            lifecycle: ExecLifecycle::Failed,
            progress_kind: None,
            flushed: false,
            waiting_on_tool_request_id: None,
            waiting_on_timer_ms: None,
            notifications: render.notifications,
            failure: Some(err.to_string()),
            cancellation: None,
            nested_tool_calls,
            truncated: false,
        });

        Some(Self::build_exec_result_from_cell(active_cell, None, None))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellDisposition {
    Continue,
    Terminal,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_mode::response::{ExecLifecycle, ExecProgressKind};

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
                None,
                Vec::new(),
                |_tool_name: String, _args_json: String| async move { Ok("null".to_string()) },
                None,
            )
            .await
            .expect("exec should publish the flush boundary");
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
                    flush({ stage: "waiting" });
                    setTimeout(() => {
                        text("done");
                    }, 1_000);
                "#,
                None,
                Vec::new(),
                |_tool_name: String, _args_json: String| async move { Ok("null".to_string()) },
                None,
            )
            .await
            .expect("initial exec should publish the explicit flush");
        assert!(summary.flushed);
        assert_eq!(summary.cell_id, "cell-0");

        let err = service
            .execute(
                "session-b",
                "text('next');",
                None,
                Vec::new(),
                |_tool_name: String, _args_json: String| async move { Ok("null".to_string()) },
                None,
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
                None,
                Vec::new(),
                |_tool_name: String, _args_json: String| async move { Ok("null".to_string()) },
                None,
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
                None,
                vec!["echo_tool".to_string()],
                |_tool_name: String, _args_json: String| async move {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    Ok(r#"{"value":"done"}"#.to_string())
                },
                None,
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
    async fn execute_surfaces_waiting_on_tool_for_long_nested_calls() {
        let service = CodeModeService::default();

        let summary = service
            .execute(
                "session-pending-tool",
                r#"
                    const response = await tools.echo_tool({ value: "done" });
                    text(response.value);
                "#,
                None,
                vec!["echo_tool".to_string()],
                |_tool_name: String, _args_json: String| async move {
                    tokio::time::sleep(std::time::Duration::from_millis(75)).await;
                    Ok(r#"{"value":"done"}"#.to_string())
                },
                None,
            )
            .await
            .expect("exec should publish the waiting-on-tool snapshot");

        assert_eq!(&summary.lifecycle, &ExecLifecycle::Running);
        assert_eq!(
            summary.waiting_on_tool_request_id.as_deref(),
            Some("echo_tool-1")
        );
        assert!(summary
            .render_output()
            .contains("processing nested tool request"));

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let final_summary = service
            .wait_with_request("session-pending-tool", Some(&summary.cell_id), Some(0))
            .await
            .expect("wait should observe the terminal summary");

        assert_eq!(&final_summary.lifecycle, &ExecLifecycle::Completed);
        assert_eq!(final_summary.output_text, "done");
    }

    #[tokio::test]
    async fn timer_boundaries_stay_internal_until_completion() {
        let service = CodeModeService::default();

        let summary = service
            .execute(
                "session-d",
                r#"
                    setTimeout(() => {
                        text("timer done");
                    }, 20);
                "#,
                None,
                Vec::new(),
                |_tool_name: String, _args_json: String| async move { Ok("null".to_string()) },
                None,
            )
            .await
            .expect("exec should wait for completion when there is no progress publication");

        assert!(!summary.flushed);
        assert_eq!(&summary.lifecycle, &ExecLifecycle::Completed);
        assert_eq!(summary.output_text, "timer done");
    }

    #[tokio::test]
    async fn auto_flush_publishes_progress_while_timer_runs() {
        let service = CodeModeService::default();

        let summary = service
            .execute(
                "session-e",
                r#"
                    text("starting");
                    setTimeout(() => {
                        text("timer done");
                    }, 40);
                "#,
                Some(10),
                Vec::new(),
                |_tool_name: String, _args_json: String| async move { Ok("null".to_string()) },
                None,
            )
            .await
            .expect("exec should auto-publish progress while the timer is pending");

        assert!(summary.flushed);
        assert_eq!(
            summary.progress_kind.as_ref(),
            Some(&ExecProgressKind::AutoFlush)
        );
        assert_eq!(&summary.lifecycle, &ExecLifecycle::Running);
        assert_eq!(summary.output_text, "starting");

        tokio::time::sleep(std::time::Duration::from_millis(60)).await;

        let final_summary = service
            .wait_with_request("session-e", Some(&summary.cell_id), Some(50))
            .await
            .expect("wait should observe completion after the timer fires");

        assert!(!final_summary.flushed);
        assert_eq!(&final_summary.lifecycle, &ExecLifecycle::Completed);
        assert!(final_summary.output_text.contains("timer done"));
    }
}
