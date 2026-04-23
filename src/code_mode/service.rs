use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::oneshot;
use tokio::sync::Mutex;
use tokio::sync::Notify;

use super::cell::{ActiveCellHandle, CellSnapshot};
use super::driver::{CellDriver, CellDriverControl, CellStatus, DriverUpdate};
use super::protocol::RuntimeEvent;
use super::response::{ExecLifecycle, ExecProgressKind, ExecRunResult};
use super::runtime;
use crate::trace::TraceStatus;

#[derive(Debug, Default, Clone)]
pub struct CodeModeService {
    sessions: Arc<Mutex<HashMap<String, SessionState>>>,
}

type SharedCellDriver = Arc<Mutex<CellDriver>>;
type SharedCellHost = Arc<CellHostHandle>;
type InitialSummaryTx = Option<oneshot::Sender<Result<ExecRunResult, crate::tools::ToolError>>>;
type CellTraceFinish = (TraceStatus, Option<String>, serde_json::Value);

const INITIAL_NESTED_TOOL_PUBLICATION_DELAY: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy)]
pub(crate) struct CellRuntimeBudget {
    pub timeout_ms: u64,
    pub deadline: Instant,
}

#[derive(Debug, Clone)]
pub(crate) struct CellExecutionOptions {
    pub auto_flush_ms: Option<u64>,
    pub runtime: CellRuntimeBudget,
    pub timeout_notice: Option<String>,
}

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

    fn observe_events(&mut self, events: &[RuntimeEvent]) -> Option<Option<Value>> {
        let mut explicit_flush_value = None;
        for event in events {
            match event {
                RuntimeEvent::Text { seq, .. } | RuntimeEvent::Notification { seq, .. } => {
                    self.latest_progress_seq = self.latest_progress_seq.max(*seq);
                }
                RuntimeEvent::Flush { seq, value } => {
                    explicit_flush_value = Some(value.clone());
                    self.latest_progress_seq = self.latest_progress_seq.max(*seq);
                }
                _ => {}
            }
        }

        explicit_flush_value
    }
}

fn clone_tool_error(error: &crate::tools::ToolError) -> crate::tools::ToolError {
    match error {
        crate::tools::ToolError::ExecutionFailed(message) => {
            crate::tools::ToolError::ExecutionFailed(message.clone())
        }
        crate::tools::ToolError::InvalidArguments(message) => {
            crate::tools::ToolError::InvalidArguments(message.clone())
        }
        crate::tools::ToolError::Timeout => crate::tools::ToolError::Timeout,
        crate::tools::ToolError::Cancelled(reason) => {
            crate::tools::ToolError::Cancelled(reason.clone())
        }
        crate::tools::ToolError::IoError(err) => {
            crate::tools::ToolError::IoError(std::io::Error::new(err.kind(), err.to_string()))
        }
    }
}

fn publish_initial(
    tx: &mut Option<oneshot::Sender<Result<ExecRunResult, crate::tools::ToolError>>>,
    summary: &ExecRunResult,
) {
    if let Some(tx) = tx.take() {
        let _ = tx.send(Ok(summary.clone()));
    }
}

fn fail_initial(
    tx: &mut Option<oneshot::Sender<Result<ExecRunResult, crate::tools::ToolError>>>,
    error: &crate::tools::ToolError,
) {
    if let Some(tx) = tx.take() {
        let _ = tx.send(Err(clone_tool_error(error)));
    }
}

pub(crate) type HostBuilder = Box<
    dyn FnOnce(
            String,
            tokio::sync::mpsc::UnboundedSender<crate::code_mode::protocol::RuntimeEvent>,
            Arc<std::sync::atomic::AtomicBool>,
        ) -> Arc<dyn crate::code_mode::host::CellRuntimeHost>
        + Send,
>;

impl CodeModeService {
    /// Execute a new code-mode cell. Spawns a live JS runtime worker, performs
    /// an initial background update, and returns the first published
    /// `ExecRunResult` suitable for the LLM.
    pub(crate) async fn execute(
        &self,
        session_id: &str,
        code: &str,
        host_builder: HostBuilder,
        options: CellExecutionOptions,
        cell_span: Option<crate::trace::TraceSpanHandle>,
    ) -> Result<ExecRunResult, crate::tools::ToolError> {
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
            let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
            let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let host = host_builder(cell_id.clone(), event_tx.clone(), cancel_flag.clone());
            let driver = CellDriver::spawn_live_with_host(
                code.to_string(),
                session.stored_values.clone(),
                host,
                event_tx,
                event_rx,
                cancel_flag,
            );
            let driver_control = driver.control_handle();
            let driver_handle = Arc::new(Mutex::new(driver));
            let host_handle = Arc::new(CellHostHandle::new(driver_handle, driver_control));
            session.active_cell = Some(ActiveCellHandle::new(
                cell_id.clone(),
                options.timeout_notice.clone(),
            ));
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
                    options,
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
                let summary = active_cell.snapshot().to_exec_result(None, None);
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

    async fn run_cell_host(
        self,
        session_id: String,
        cell_id: String,
        host_handle: SharedCellHost,
        options: CellExecutionOptions,
        initial_summary_tx: oneshot::Sender<Result<ExecRunResult, crate::tools::ToolError>>,
        mut cell_span: Option<crate::trace::TraceSpanHandle>,
    ) {
        let mut initial_summary_tx = Some(initial_summary_tx);
        let trace_finish = self
            .perform_cell_host_loop(
                &session_id,
                &cell_id,
                &host_handle,
                options,
                &mut initial_summary_tx,
            )
            .await;

        if let Some(span) = cell_span.take() {
            let (status, summary, attrs) = trace_finish;
            span.finish("code_mode_cell_finished", status, summary, attrs);
        }
    }

    async fn perform_cell_host_loop(
        &self,
        session_id: &str,
        cell_id: &str,
        host_handle: &SharedCellHost,
        options: CellExecutionOptions,
        initial_summary_tx: &mut InitialSummaryTx,
    ) -> CellTraceFinish {
        let mut publication_tracker = PublicationTracker::new(options.auto_flush_ms);
        let mut pending_initial_tool_summary: Option<(ExecRunResult, Instant)> = None;

        loop {
            let mut idle_timeout = publication_tracker.next_idle_timeout();
            let now = Instant::now();
            if now >= options.runtime.deadline {
                return self
                    .cancel_cell_for_runtime_deadline(
                        session_id,
                        cell_id,
                        host_handle,
                        initial_summary_tx,
                        options.runtime.timeout_ms,
                    )
                    .await;
            }
            let until_cell_deadline = options.runtime.deadline.saturating_duration_since(now);
            idle_timeout = Some(match idle_timeout {
                Some(existing) => existing.min(until_cell_deadline),
                None => until_cell_deadline,
            });
            if let Some((_, publish_at)) = pending_initial_tool_summary.as_ref() {
                let until_initial_publish = publish_at.saturating_duration_since(Instant::now());
                idle_timeout = Some(match idle_timeout {
                    Some(existing) => existing.min(until_initial_publish),
                    None => until_initial_publish,
                });
            }

            let driver_update = {
                let mut driver = host_handle.driver_handle.lock().await;
                driver.next_update(idle_timeout).await
            };

            let update = match driver_update {
                Ok(update) => update,
                Err(err) => {
                    return self
                        .record_and_exit_host_error(
                            session_id,
                            cell_id,
                            host_handle,
                            initial_summary_tx,
                            err,
                        )
                        .await;
                }
            };

            if matches!(update.status, CellStatus::Idle) {
                if let Some((summary, publish_at)) = pending_initial_tool_summary.as_ref() {
                    if initial_summary_tx.is_some() && Instant::now() >= *publish_at {
                        let summary = summary.clone();
                        pending_initial_tool_summary = None;
                        if let Err(err) = self
                            .publish_summary_and_unblock_initial(
                                session_id,
                                cell_id,
                                host_handle,
                                initial_summary_tx,
                                &summary,
                            )
                            .await
                        {
                            return Self::fail_and_exit(cell_id, initial_summary_tx, err);
                        }
                        continue;
                    }
                }

                if !publication_tracker.should_auto_flush_now() {
                    continue;
                }

                let summary = match self
                    .peek_cell_summary(session_id, cell_id, Some(ExecProgressKind::AutoFlush), None)
                    .await
                {
                    Ok(summary) => summary,
                    Err(err) => return Self::fail_and_exit(cell_id, initial_summary_tx, err),
                };
                publication_tracker.mark_published();

                if let Err(err) = self
                    .publish_summary_and_unblock_initial(
                        session_id,
                        cell_id,
                        host_handle,
                        initial_summary_tx,
                        &summary,
                    )
                    .await
                {
                    return Self::fail_and_exit(cell_id, initial_summary_tx, err);
                }
                continue;
            }

            let explicit_flush_value = publication_tracker.observe_events(&update.events);

            let snapshot = match self
                .record_driver_update_in_session(session_id, cell_id, &update)
                .await
            {
                Ok(snapshot) => snapshot,
                Err(err) => return Self::fail_and_exit(cell_id, initial_summary_tx, err),
            };

            match update.status {
                CellStatus::Progress => {
                    let publication = if let Some(flush_value) = explicit_flush_value {
                        publication_tracker.mark_published();
                        Some(
                            snapshot
                                .to_exec_result(Some(ExecProgressKind::ExplicitFlush), flush_value),
                        )
                    } else if publication_tracker.should_auto_flush_now() {
                        publication_tracker.mark_published();
                        Some(snapshot.to_exec_result(Some(ExecProgressKind::AutoFlush), None))
                    } else {
                        None
                    };

                    if let Some(summary) = publication {
                        pending_initial_tool_summary = None;
                        if let Err(err) = self
                            .publish_summary_and_unblock_initial(
                                session_id,
                                cell_id,
                                host_handle,
                                initial_summary_tx,
                                &summary,
                            )
                            .await
                        {
                            return Self::fail_and_exit(cell_id, initial_summary_tx, err);
                        }
                    }
                }
                CellStatus::PendingTool => {
                    if snapshot.lifecycle() != ExecLifecycle::Running {
                        let err = crate::tools::ToolError::ExecutionFailed(
                            "Code mode entered a terminal state while dispatching a nested tool."
                                .to_string(),
                        );
                        return Self::fail_and_exit(cell_id, initial_summary_tx, err);
                    }

                    let current_summary = snapshot.to_exec_result(None, None);
                    if let Err(err) = self
                        .publish_summary_update(session_id, cell_id, host_handle, &current_summary)
                        .await
                    {
                        return Self::fail_and_exit(cell_id, initial_summary_tx, err);
                    }
                    if initial_summary_tx.is_some() {
                        pending_initial_tool_summary = Some((
                            current_summary,
                            Instant::now() + INITIAL_NESTED_TOOL_PUBLICATION_DELAY,
                        ));
                    }
                }
                CellStatus::Terminal(_) => {
                    let summary = snapshot.to_exec_result(None, None);
                    if let Err(err) = self
                        .publish_summary_and_unblock_initial(
                            session_id,
                            cell_id,
                            host_handle,
                            initial_summary_tx,
                            &summary,
                        )
                        .await
                    {
                        return Self::fail_and_exit(cell_id, initial_summary_tx, err);
                    }

                    return Self::trace_finish_from_summary(cell_id, &summary, None);
                }
                CellStatus::Idle => {}
            }
        }
    }

    async fn record_driver_update_in_session(
        &self,
        session_id: &str,
        cell_id: &str,
        update: &DriverUpdate,
    ) -> Result<CellSnapshot, crate::tools::ToolError> {
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
        active_cell.record_driver_update(update);

        if let CellStatus::Terminal(terminal_result) = &update.status {
            session.stored_values = terminal_result.stored_values.clone();
        }

        Ok(active_cell.snapshot())
    }

    async fn record_cell_cancellation_in_session(
        &self,
        session_id: &str,
        cell_id: &str,
        reason: String,
    ) -> Result<CellSnapshot, crate::tools::ToolError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions.get_mut(session_id).ok_or_else(|| {
            crate::tools::ToolError::ExecutionFailed("Session disappeared during wait.".to_string())
        })?;

        let active_cell = session.active_cell.as_mut().ok_or_else(|| {
            crate::tools::ToolError::ExecutionFailed(format!(
                "Code mode cell `{}` was terminated before cancellation was recorded.",
                cell_id
            ))
        })?;
        if active_cell.cell_id != cell_id {
            return Err(crate::tools::ToolError::ExecutionFailed(format!(
                "Code mode cell `{}` was superseded before cancellation was recorded.",
                cell_id
            )));
        }

        active_cell.transition_to_cancelled(reason);
        Ok(active_cell.snapshot())
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

        Ok(active_cell
            .snapshot()
            .to_exec_result(progress_kind, flush_value))
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
                .unwrap_or_else(|| active_cell.snapshot().to_exec_result(None, None))
        } else {
            active_cell.snapshot().to_exec_result(None, None)
        };
        if active_cell.is_terminal() {
            session.active_cell = None;
            session.host_handle = None;
        }

        Ok(summary)
    }

    fn fail_and_exit(
        cell_id: &str,
        initial_summary_tx: &mut InitialSummaryTx,
        error: crate::tools::ToolError,
    ) -> CellTraceFinish {
        fail_initial(initial_summary_tx, &error);
        Self::trace_finish_from_host_error(cell_id, &error)
    }

    async fn record_and_exit_host_error(
        &self,
        session_id: &str,
        cell_id: &str,
        host_handle: &SharedCellHost,
        initial_summary_tx: &mut InitialSummaryTx,
        error: crate::tools::ToolError,
    ) -> CellTraceFinish {
        let summary = self
            .record_host_error_in_session(session_id, cell_id, &error)
            .await;

        match summary.as_ref() {
            Some(summary) => {
                if let Err(err) = self
                    .publish_summary_update(session_id, cell_id, host_handle, summary)
                    .await
                {
                    tracing::warn!("Failed to publish host-error summary update: {}", err);
                }
                publish_initial(initial_summary_tx, summary);
                Self::trace_finish_from_summary(cell_id, summary, Some(&error))
            }
            None => {
                fail_initial(initial_summary_tx, &error);
                Self::trace_finish_from_host_error(cell_id, &error)
            }
        }
    }

    async fn cancel_cell_for_runtime_deadline(
        &self,
        session_id: &str,
        cell_id: &str,
        host_handle: &SharedCellHost,
        initial_summary_tx: &mut InitialSummaryTx,
        cell_timeout_ms: u64,
    ) -> CellTraceFinish {
        let reason = format!(
            "Code mode cell exceeded its runtime limit of {}ms and was cancelled by the system.",
            cell_timeout_ms
        );
        host_handle.driver_control.request_cancel(&reason);

        let snapshot = match self
            .record_cell_cancellation_in_session(session_id, cell_id, reason)
            .await
        {
            Ok(snapshot) => snapshot,
            Err(err) => return Self::fail_and_exit(cell_id, initial_summary_tx, err),
        };
        let summary = snapshot.to_exec_result(None, None);

        if let Err(err) = self
            .publish_summary_and_unblock_initial(
                session_id,
                cell_id,
                host_handle,
                initial_summary_tx,
                &summary,
            )
            .await
        {
            return Self::fail_and_exit(cell_id, initial_summary_tx, err);
        }

        Self::trace_finish_from_summary(cell_id, &summary, None)
    }

    async fn publish_summary_and_unblock_initial(
        &self,
        session_id: &str,
        cell_id: &str,
        host_handle: &SharedCellHost,
        initial_summary_tx: &mut InitialSummaryTx,
        summary: &ExecRunResult,
    ) -> Result<(), crate::tools::ToolError> {
        self.publish_summary_update(session_id, cell_id, host_handle, summary)
            .await?;
        publish_initial(initial_summary_tx, summary);
        Ok(())
    }

    async fn publish_summary_update(
        &self,
        session_id: &str,
        cell_id: &str,
        host_handle: &SharedCellHost,
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
                "No active code-mode cell to publish progress for.".to_string(),
            )
        })?;
        if active_cell.cell_id != cell_id {
            return Err(crate::tools::ToolError::ExecutionFailed(format!(
                "Code mode cell `{}` was superseded before the publication summary was stored.",
                cell_id
            )));
        }

        active_cell.last_publication = Some(summary.clone());
        host_handle.publish_update();
        Ok(())
    }

    fn trace_finish_from_summary(
        cell_id: &str,
        summary: &ExecRunResult,
        host_error: Option<&crate::tools::ToolError>,
    ) -> CellTraceFinish {
        let status = match &summary.lifecycle {
            ExecLifecycle::Completed | ExecLifecycle::Running => TraceStatus::Ok,
            ExecLifecycle::Failed => TraceStatus::Error,
            ExecLifecycle::Cancelled => TraceStatus::Cancelled,
        };
        let trace_summary = match &summary.lifecycle {
            ExecLifecycle::Completed => Some("completed".to_string()),
            ExecLifecycle::Failed => summary.failure.clone().or(Some("failed".to_string())),
            ExecLifecycle::Cancelled => summary
                .cancellation
                .clone()
                .or(Some("cancelled".to_string())),
            ExecLifecycle::Running => None,
        };

        let mut attrs = serde_json::json!({
            "cell_id": cell_id,
            "lifecycle": summary.lifecycle.clone(),
            "nested_tool_calls": summary.nested_tool_calls,
        });
        if let Some(error) = host_error {
            attrs["failure"] = serde_json::json!(summary.failure.clone());
            attrs["cancellation"] = serde_json::json!(summary.cancellation.clone());
            attrs["error"] = serde_json::json!(error.to_string());
        }

        (status, trace_summary, attrs)
    }

    fn trace_finish_from_host_error(
        cell_id: &str,
        error: &crate::tools::ToolError,
    ) -> CellTraceFinish {
        let status = match error {
            crate::tools::ToolError::Timeout => TraceStatus::TimedOut,
            crate::tools::ToolError::Cancelled(_) => TraceStatus::Cancelled,
            _ => TraceStatus::Error,
        };
        (
            status,
            Some(error.to_string()),
            serde_json::json!({
                "cell_id": cell_id,
                "error": error.to_string(),
            }),
        )
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

        active_cell.transition_to_failure(err.to_string());
        Some(active_cell.snapshot().to_exec_result(None, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_mode::response::{ExecLifecycle, ExecProgressKind};
    use crate::tools::{Tool, ToolContext, ToolError};
    use async_trait::async_trait;

    struct DelayedEchoTool {
        delay_ms: u64,
    }

    #[async_trait]
    impl Tool for DelayedEchoTool {
        fn name(&self) -> String {
            "echo_tool".to_string()
        }

        fn description(&self) -> String {
            "echo".to_string()
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }

        fn has_side_effects(&self) -> bool {
            false
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<String, ToolError> {
            if self.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            }
            Ok(r#"{"value":"done"}"#.to_string())
        }
    }

    fn host_factory(
        visible_tools: Vec<String>,
        current_tools: Vec<Arc<dyn Tool>>,
    ) -> crate::code_mode::service::HostBuilder {
        host_factory_with_deadline(
            visible_tools,
            current_tools,
            Instant::now() + Duration::from_secs(120),
        )
    }

    fn host_factory_with_deadline(
        visible_tools: Vec<String>,
        current_tools: Vec<Arc<dyn Tool>>,
        cell_deadline: Instant,
    ) -> crate::code_mode::service::HostBuilder {
        let executor = crate::tools::invocation::UnifiedToolExecutor::new(
            crate::tools::invocation::UnifiedToolExecutorConfig {
                current_tools,
                visible_tools: visible_tools.clone(),
                extensions: Vec::new(),
                session_id: "service-test-session".to_string(),
                reply_to: "test".to_string(),
                step_budget: crate::tools::invocation::StepBudgetHandle::new(10),
                session_deadline: None,
                cancel_token: Arc::new(Notify::new()),
                is_autopilot: false,
                todos_path: std::path::PathBuf::from("TODOS.md"),
                execution_guard_state: Arc::new(std::sync::Mutex::new(
                    crate::core::ExecutionGuardState::default(),
                )),
            },
        );
        crate::code_mode::host::create_executor_host_builder(
            visible_tools,
            Arc::new(Mutex::new(executor)),
            crate::code_mode::host::ExecutorHostConfig {
                trace_bus: crate::trace::shared_bus(),
                trace_ctx: None,
                parent_span_id: None,
                outer_tool_call_id: None,
                provider: "test-provider".to_string(),
                model: "test-model".to_string(),
                cell_deadline,
            },
        )
    }

    fn default_test_cell_timeout_ms() -> u64 {
        120_000
    }

    fn default_test_cell_deadline() -> Instant {
        Instant::now() + Duration::from_secs(120)
    }

    fn test_cell_options(
        auto_flush_ms: Option<u64>,
        timeout_ms: u64,
        deadline: Instant,
    ) -> CellExecutionOptions {
        CellExecutionOptions {
            auto_flush_ms,
            runtime: CellRuntimeBudget {
                timeout_ms,
                deadline,
            },
            timeout_notice: None,
        }
    }

    fn default_test_cell_options(auto_flush_ms: Option<u64>) -> CellExecutionOptions {
        test_cell_options(
            auto_flush_ms,
            default_test_cell_timeout_ms(),
            default_test_cell_deadline(),
        )
    }

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
                host_factory(Vec::new(), Vec::new()),
                default_test_cell_options(None),
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
                host_factory(Vec::new(), Vec::new()),
                default_test_cell_options(None),
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
                host_factory(Vec::new(), Vec::new()),
                default_test_cell_options(None),
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
                host_factory(Vec::new(), Vec::new()),
                default_test_cell_options(None),
                None,
            ),
        )
        .await;

        assert!(exec_result.is_err(), "execute should time out");

        let cancelled = service.abort_active_cell("session-loop", "Timeout").await;
        assert!(cancelled, "should successfully abort active cell");
    }

    #[tokio::test]
    async fn execute_cancels_cell_at_runtime_deadline() {
        let service = CodeModeService::default();
        let timeout_ms = 30;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);

        let summary = service
            .execute(
                "session-deadline",
                r#"
                    setTimeout(() => {
                        text("too late");
                    }, 1_000);
                "#,
                host_factory_with_deadline(Vec::new(), Vec::new(), deadline),
                test_cell_options(None, timeout_ms, deadline),
                None,
            )
            .await
            .expect("exec should publish a cancellation summary at the runtime deadline");

        assert_eq!(&summary.lifecycle, &ExecLifecycle::Cancelled);
        assert!(
            summary
                .cancellation
                .as_deref()
                .unwrap_or_default()
                .contains("runtime limit"),
            "unexpected cancellation: {:?}",
            summary.cancellation
        );
    }

    #[tokio::test]
    async fn cell_deadline_bounds_nested_tool_runtime() {
        let service = CodeModeService::default();
        let timeout_ms = 40;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);

        let summary = service
            .execute(
                "session-nested-deadline",
                r#"
                    const response = await tools.echo_tool({ value: "done" });
                    text(response.value);
                "#,
                host_factory_with_deadline(
                    vec!["echo_tool".to_string()],
                    vec![Arc::new(DelayedEchoTool { delay_ms: 250 })],
                    deadline,
                ),
                test_cell_options(None, timeout_ms, deadline),
                None,
            )
            .await
            .expect("exec should publish a cancellation summary at the runtime deadline");

        assert_eq!(&summary.lifecycle, &ExecLifecycle::Cancelled);
        assert!(
            summary
                .cancellation
                .as_deref()
                .unwrap_or_default()
                .contains("runtime limit"),
            "unexpected cancellation: {:?}",
            summary.cancellation
        );
    }

    #[tokio::test]
    async fn wait_without_timeout_wakes_when_cell_deadline_cancels() {
        let service = CodeModeService::default();
        let timeout_ms = 40;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);

        let summary = service
            .execute(
                "session-wait-deadline",
                r#"
                    flush({ stage: "started" });
                    setTimeout(() => {
                        text("too late");
                    }, 1_000);
                "#,
                host_factory_with_deadline(Vec::new(), Vec::new(), deadline),
                test_cell_options(None, timeout_ms, deadline),
                None,
            )
            .await
            .expect("exec should publish the initial flush");

        assert!(summary.flushed);
        assert_eq!(&summary.lifecycle, &ExecLifecycle::Running);

        let waited = tokio::time::timeout(
            Duration::from_millis(250),
            service.wait_with_request("session-wait-deadline", Some(&summary.cell_id), None),
        )
        .await
        .expect("wait should wake at the cell runtime deadline")
        .expect("wait should return the cancellation summary");

        assert_eq!(&waited.lifecycle, &ExecLifecycle::Cancelled);
        assert!(
            waited
                .cancellation
                .as_deref()
                .unwrap_or_default()
                .contains("runtime limit"),
            "unexpected cancellation: {:?}",
            waited.cancellation
        );
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
                host_factory(
                    vec!["echo_tool".to_string()],
                    vec![Arc::new(DelayedEchoTool { delay_ms: 25 })],
                ),
                default_test_cell_options(None),
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
                host_factory(
                    vec!["echo_tool".to_string()],
                    vec![Arc::new(DelayedEchoTool { delay_ms: 250 })],
                ),
                default_test_cell_options(None),
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

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

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
                host_factory(Vec::new(), Vec::new()),
                default_test_cell_options(None),
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
                host_factory(Vec::new(), Vec::new()),
                default_test_cell_options(Some(10)),
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
