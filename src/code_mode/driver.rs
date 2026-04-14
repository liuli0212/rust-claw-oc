use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant as TokioInstant;

use super::cell::CellResumeProgressDelta;
use super::protocol::{
    CellCommand, DrainRequest, RuntimeCellResult, RuntimeEvent, ToolCallRequest,
};
use super::response::{timer_pending_resume_after_ms, ExecYieldKind};
use super::runtime;
use super::runtime::callbacks::RecordedToolCall;

#[derive(Debug)]
pub struct CellDriver {
    event_rx: tokio::sync::mpsc::UnboundedReceiver<RuntimeEvent>,
    command_tx: std::sync::mpsc::Sender<CellCommand>,
    worker: Option<tokio::task::JoinHandle<()>>,
    resume_progress: Arc<Mutex<CellResumeProgressDelta>>,
}

pub struct DriverCompletion {
    pub runtime_result: RuntimeCellResult,
    pub events: Vec<RuntimeEvent>,
}

#[derive(Debug)]
pub struct DriverDrainBatch {
    pub request: DrainRequest,
    pub terminal_result: Option<RuntimeCellResult>,
    pub events: Vec<RuntimeEvent>,
    pub resume_progress: CellResumeProgressDelta,
}

impl DriverDrainBatch {
    pub fn empty(request: DrainRequest) -> Self {
        Self::progress(request, Vec::new())
    }

    pub fn progress(request: DrainRequest, events: Vec<RuntimeEvent>) -> Self {
        let resume_progress = resume_progress_from_events(&events);
        Self {
            request,
            terminal_result: None,
            events,
            resume_progress,
        }
    }

    pub fn terminal(
        request: DrainRequest,
        runtime_result: RuntimeCellResult,
        events: Vec<RuntimeEvent>,
    ) -> Self {
        Self {
            request,
            terminal_result: Some(runtime_result),
            events,
            resume_progress: CellResumeProgressDelta::default(),
        }
    }

    pub fn with_resume_progress(mut self, resume_progress: CellResumeProgressDelta) -> Self {
        self.resume_progress = resume_progress;
        self
    }

    pub fn into_completion(self) -> Result<DriverCompletion, crate::tools::ToolError> {
        Ok(DriverCompletion {
            runtime_result: self.terminal_result.ok_or_else(|| {
                crate::tools::ToolError::ExecutionFailed(
                    "Code mode drain batch did not include a terminal completion.".to_string(),
                )
            })?,
            events: self.events,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn requested_wait_for_event(&self) -> bool {
        self.request.wait_for_event
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriverMode {
    OneShot,
    Live { suppress_initial_timer_yield: bool },
}

type SharedCommandReceiver = Arc<Mutex<std::sync::mpsc::Receiver<CellCommand>>>;

#[derive(Clone)]
struct WorkerRuntimeState {
    command_rx: SharedCommandReceiver,
    event_tx: tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
    next_request_id: Arc<AtomicU64>,
    next_seq: Arc<AtomicU64>,
    resume_progress: Arc<Mutex<CellResumeProgressDelta>>,
}

impl CellDriver {
    pub fn spawn(
        cell_id: String,
        code: String,
        visible_tools: Vec<String>,
        stored_values: HashMap<String, serde_json::Value>,
        resume_state: runtime::ResumeState,
    ) -> Self {
        Self::spawn_with_mode(
            cell_id,
            code,
            visible_tools,
            stored_values,
            resume_state,
            DriverMode::OneShot,
        )
    }

    pub fn spawn_live(
        cell_id: String,
        code: String,
        visible_tools: Vec<String>,
        stored_values: HashMap<String, serde_json::Value>,
        resume_state: runtime::ResumeState,
        suppress_initial_timer_yield: bool,
    ) -> Self {
        Self::spawn_with_mode(
            cell_id,
            code,
            visible_tools,
            stored_values,
            resume_state,
            DriverMode::Live {
                suppress_initial_timer_yield,
            },
        )
    }

    fn spawn_with_mode(
        cell_id: String,
        code: String,
        visible_tools: Vec<String>,
        stored_values: HashMap<String, serde_json::Value>,
        resume_state: runtime::ResumeState,
        mode: DriverMode,
    ) -> Self {
        let handle = tokio::runtime::Handle::current();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<RuntimeEvent>();
        let (command_tx, command_rx) = std::sync::mpsc::channel::<CellCommand>();
        let command_rx = Arc::new(Mutex::new(command_rx));
        let resume_progress = Arc::new(Mutex::new(CellResumeProgressDelta::default()));
        let resume_progress_for_runtime = resume_progress.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let runtime_event_tx = event_tx.clone();
            let completion_event_tx = event_tx;
            let worker_state = WorkerRuntimeState {
                command_rx,
                event_tx: runtime_event_tx.clone(),
                next_request_id: Arc::new(AtomicU64::new(0)),
                next_seq: Arc::new(AtomicU64::new(0)),
                resume_progress: resume_progress_for_runtime,
            };
            let next_seq = worker_state.next_seq.clone();
            let result = match mode {
                DriverMode::OneShot => run_worker_iteration(
                    handle,
                    runtime::RunCellRequest {
                        cell_id,
                        code,
                        visible_tools,
                        stored_values,
                        resume_state,
                    },
                    worker_state.clone(),
                ),
                DriverMode::Live {
                    suppress_initial_timer_yield,
                } => run_live_worker(
                    handle,
                    runtime::RunCellRequest {
                        cell_id,
                        code,
                        visible_tools,
                        stored_values,
                        resume_state,
                    },
                    worker_state,
                    suppress_initial_timer_yield,
                ),
            };
            emit_summary_events(&runtime_event_tx, next_seq.as_ref(), &result);
            let _ = completion_event_tx.send(RuntimeEvent::WorkerCompleted(result));
        });

        Self {
            event_rx,
            command_tx,
            worker: Some(worker),
            resume_progress,
        }
    }

    pub async fn drive_to_completion<F, Fut>(
        &mut self,
        invoke_tool: &mut F,
    ) -> Result<RuntimeCellResult, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        Ok(self
            .drive_to_completion_with_events(invoke_tool)
            .await?
            .runtime_result)
    }

    pub async fn drive_to_completion_with_events<F, Fut>(
        &mut self,
        invoke_tool: &mut F,
    ) -> Result<DriverCompletion, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        self.drain_event_batch(invoke_tool).await?.into_completion()
    }

    pub async fn drain_event_batch<F, Fut>(
        &mut self,
        invoke_tool: &mut F,
    ) -> Result<DriverDrainBatch, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        self.drain_event_batch_with_request(DrainRequest::to_completion(), invoke_tool)
            .await
    }

    pub async fn drain_event_batch_with_request<F, Fut>(
        &mut self,
        request: DrainRequest,
        invoke_tool: &mut F,
    ) -> Result<DriverDrainBatch, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        if !request.wait_for_event {
            return self.drain_to_completion_batch(request, invoke_tool).await;
        }

        if request.wait_for_event {
            let _ = self.command_tx.send(CellCommand::Drain(request));
        }

        if request.wait_timeout_ms == Some(0) {
            return self.drain_available_event_batch(request, invoke_tool).await;
        }

        let mut events = Vec::new();
        let refresh_deadline = request
            .refresh_slice_ms
            .map(|refresh_ms| TokioInstant::now() + Duration::from_millis(refresh_ms));
        let mut saw_visible_event = false;
        let mut saw_terminal_summary = false;

        loop {
            let drained = self
                .drain_buffered_events(
                    request,
                    &mut events,
                    invoke_tool,
                    &mut saw_visible_event,
                    &mut saw_terminal_summary,
                )
                .await?;
            if let Some(batch) = drained {
                return Ok(batch);
            }

            let next_event = if saw_terminal_summary {
                self.event_rx.recv().await
            } else if saw_visible_event {
                match refresh_deadline {
                    Some(deadline) if TokioInstant::now() < deadline => {
                        match tokio::time::timeout_at(deadline, self.event_rx.recv()).await {
                            Ok(event) => event,
                            Err(_) => return Ok(self.finish_progress_batch(request, events)),
                        }
                    }
                    Some(_) | None => return Ok(self.finish_progress_batch(request, events)),
                }
            } else if let Some(wait_timeout_ms) = request.wait_timeout_ms {
                match tokio::time::timeout(
                    Duration::from_millis(wait_timeout_ms),
                    self.event_rx.recv(),
                )
                .await
                {
                    Ok(event) => event,
                    Err(_) => return Ok(self.finish_progress_batch(request, events)),
                }
            } else {
                self.event_rx.recv().await
            };

            let Some(event) = next_event else {
                self.await_worker().await?;
                return Ok(self.finish_progress_batch(request, events));
            };
            if let Some(batch) = self
                .process_runtime_event(
                    request,
                    &mut events,
                    event,
                    invoke_tool,
                    &mut saw_visible_event,
                    &mut saw_terminal_summary,
                )
                .await?
            {
                return Ok(batch);
            }
        }
    }

    async fn drain_to_completion_batch<F, Fut>(
        &mut self,
        request: DrainRequest,
        invoke_tool: &mut F,
    ) -> Result<DriverDrainBatch, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        let mut events = Vec::new();
        let mut saw_visible_event = false;
        let mut saw_terminal_summary = false;

        while let Some(event) = self.event_rx.recv().await {
            if let Some(batch) = self
                .process_runtime_event(
                    request,
                    &mut events,
                    event,
                    invoke_tool,
                    &mut saw_visible_event,
                    &mut saw_terminal_summary,
                )
                .await?
            {
                if batch.terminal_result.is_some() {
                    return Ok(batch);
                }
                continue;
            }
        }

        self.await_worker().await?;
        Err(crate::tools::ToolError::ExecutionFailed(
            "Code mode runtime worker exited without a completion event.".to_string(),
        ))
    }

    async fn drain_available_event_batch<F, Fut>(
        &mut self,
        request: DrainRequest,
        invoke_tool: &mut F,
    ) -> Result<DriverDrainBatch, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        let mut events = Vec::new();
        let mut saw_visible_event = false;
        let mut saw_terminal_summary = false;

        if let Some(batch) = self
            .drain_buffered_events(
                request,
                &mut events,
                invoke_tool,
                &mut saw_visible_event,
                &mut saw_terminal_summary,
            )
            .await?
        {
            return Ok(batch);
        }

        Ok(self.finish_progress_batch(request, events))
    }

    async fn drain_buffered_events<F, Fut>(
        &mut self,
        request: DrainRequest,
        events: &mut Vec<RuntimeEvent>,
        invoke_tool: &mut F,
        saw_visible_event: &mut bool,
        saw_terminal_summary: &mut bool,
    ) -> Result<Option<DriverDrainBatch>, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        loop {
            match self.event_rx.try_recv() {
                Ok(event) => {
                    if let Some(batch) = self
                        .process_runtime_event(
                            request,
                            events,
                            event,
                            invoke_tool,
                            saw_visible_event,
                            saw_terminal_summary,
                        )
                        .await?
                    {
                        return Ok(Some(batch));
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return Ok(None),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    self.await_worker().await?;
                    return Ok(Some(
                        self.finish_progress_batch(request, std::mem::take(events)),
                    ));
                }
            }
        }
    }

    async fn process_runtime_event<F, Fut>(
        &mut self,
        request: DrainRequest,
        events: &mut Vec<RuntimeEvent>,
        event: RuntimeEvent,
        invoke_tool: &mut F,
        saw_visible_event: &mut bool,
        saw_terminal_summary: &mut bool,
    ) -> Result<Option<DriverDrainBatch>, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        let visible_before = count_user_visible_events(events);
        match event {
            RuntimeEvent::ToolCallRequested(request_event) => {
                let request_id = request_event.request_id;
                let tool_name = request_event.tool_name.clone();
                let args_json = request_event.args_json.clone();
                events.push(RuntimeEvent::ToolCallRequested(request_event));
                let result = invoke_tool(tool_name.clone(), args_json.clone()).await;
                self.resume_progress
                    .lock()
                    .unwrap()
                    .replayed_tool_calls_delta
                    .push(RecordedToolCall {
                        tool_name: tool_name.clone(),
                        args_json: args_json.clone(),
                        result_json: recorded_tool_result_json(&result),
                    });
                self.command_tx
                    .send(CellCommand::ToolResult {
                        request_id,
                        outcome: result,
                    })
                    .map_err(|_| {
                        crate::tools::ToolError::ExecutionFailed(
                            "Code mode nested tool response channel closed unexpectedly."
                                .to_string(),
                        )
                    })?;
                Ok(None)
            }
            RuntimeEvent::WorkerCompleted(result) => {
                self.await_worker().await?;
                Ok(Some(DriverDrainBatch::terminal(
                    request,
                    result?,
                    std::mem::take(events),
                )))
            }
            event => {
                if event.is_visible_to_drain() {
                    events.push(event);
                }
                Ok(None)
            }
        }
        .map(|batch| {
            if batch.is_none() {
                let visible_after = count_user_visible_events(events);
                if visible_after > visible_before {
                    *saw_visible_event = true;
                    if let Some(last_visible) = last_user_visible_event(events) {
                        *saw_terminal_summary |= last_visible.is_terminal_summary_event();
                        if request.wait_for_event
                            && matches!(last_visible, RuntimeEvent::Yield { .. })
                        {
                            return Some(
                                self.finish_progress_batch(request, std::mem::take(events)),
                            );
                        }
                    }
                }
            }
            batch
        })
    }

    pub fn cancel(&self, reason: String) -> Result<(), crate::tools::ToolError> {
        self.command_tx
            .send(CellCommand::Cancel { reason })
            .map_err(|_| {
                crate::tools::ToolError::ExecutionFailed(
                    "Code mode runtime worker is no longer accepting commands.".to_string(),
                )
            })
    }

    async fn await_worker(&mut self) -> Result<(), crate::tools::ToolError> {
        match self.worker.take() {
            Some(worker) => worker.await.map_err(|err| {
                crate::tools::ToolError::ExecutionFailed(format!(
                    "Code mode runtime worker failed: {}",
                    err
                ))
            }),
            None => Ok(()),
        }
    }

    fn take_resume_progress(&self, events: &[RuntimeEvent]) -> CellResumeProgressDelta {
        let mut progress = {
            let mut resume_progress = self.resume_progress.lock().unwrap();
            std::mem::take(&mut *resume_progress)
        };
        progress.merge(resume_progress_from_events(events));
        progress
    }

    fn finish_progress_batch(
        &self,
        request: DrainRequest,
        events: Vec<RuntimeEvent>,
    ) -> DriverDrainBatch {
        let resume_progress = self.take_resume_progress(&events);
        if events.is_empty() {
            DriverDrainBatch::empty(request).with_resume_progress(resume_progress)
        } else {
            DriverDrainBatch::progress(request, events).with_resume_progress(resume_progress)
        }
    }
}

fn run_worker_iteration(
    handle: tokio::runtime::Handle,
    request: runtime::RunCellRequest,
    worker_state: WorkerRuntimeState,
) -> Result<RuntimeCellResult, crate::tools::ToolError> {
    let WorkerRuntimeState {
        command_rx,
        event_tx,
        next_request_id,
        next_seq,
        resume_progress,
    } = worker_state;
    let tool_event_tx = event_tx;
    let next_seq_for_tool = next_seq;
    runtime::run_cell(
        handle,
        request,
        move |tool_name: String, args_json: String| {
            let request_id = next_request_id.fetch_add(1, Ordering::Relaxed) + 1;
            let seq = next_seq_for_tool.fetch_add(1, Ordering::Relaxed) + 1;
            tool_event_tx
                .send(RuntimeEvent::ToolCallRequested(ToolCallRequest {
                    seq,
                    request_id,
                    tool_name,
                    args_json,
                }))
                .map_err(|_| {
                    crate::tools::ToolError::ExecutionFailed(
                        "Code mode nested tool bridge closed unexpectedly.".to_string(),
                    )
                })?;
            loop {
                match command_rx.lock().unwrap().recv().map_err(|_| {
                    crate::tools::ToolError::ExecutionFailed(
                        "Code mode nested tool response channel closed unexpectedly.".to_string(),
                    )
                })? {
                    CellCommand::ToolResult {
                        request_id: response_id,
                        outcome,
                    } if response_id == request_id => {
                        let ok = outcome.is_ok();
                        let seq = next_seq_for_tool.fetch_add(1, Ordering::Relaxed) + 1;
                        let _ = tool_event_tx.send(RuntimeEvent::ToolCallResolved {
                            seq,
                            request_id,
                            ok,
                        });
                        break outcome;
                    }
                    CellCommand::ToolResult { .. } => continue,
                    CellCommand::Drain(_) => continue,
                    CellCommand::Cancel { reason } => {
                        return Err(crate::tools::ToolError::ExecutionFailed(reason));
                    }
                }
            }
        },
        move |timer_calls| {
            resume_progress.lock().unwrap().recorded_timer_calls = Some(timer_calls);
        },
    )
}

fn run_live_worker(
    handle: tokio::runtime::Handle,
    request: runtime::RunCellRequest,
    worker_state: WorkerRuntimeState,
    mut suppress_initial_timer_yield: bool,
) -> Result<RuntimeCellResult, crate::tools::ToolError> {
    let runtime::RunCellRequest {
        cell_id,
        code,
        visible_tools,
        mut stored_values,
        mut resume_state,
    } = request;
    let WorkerRuntimeState {
        command_rx,
        event_tx,
        next_request_id,
        next_seq,
        resume_progress,
    } = worker_state;

    loop {
        let result = run_worker_iteration(
            handle.clone(),
            runtime::RunCellRequest {
                cell_id: cell_id.clone(),
                code: code.clone(),
                visible_tools: visible_tools.clone(),
                stored_values: stored_values.clone(),
                resume_state: resume_state.clone(),
            },
            WorkerRuntimeState {
                command_rx: command_rx.clone(),
                event_tx: event_tx.clone(),
                next_request_id: next_request_id.clone(),
                next_seq: next_seq.clone(),
                resume_progress: resume_progress.clone(),
            },
        )?;

        let (summary, next_stored_values, metadata) = result;
        if !summary.yielded {
            return Ok((summary, next_stored_values, metadata));
        }

        let should_suppress_timer_yield = suppress_initial_timer_yield
            && matches!(summary.yield_kind, Some(ExecYieldKind::Timer))
            && summary.output_text.trim().is_empty()
            && summary.notifications.is_empty();
        suppress_initial_timer_yield = false;
        if !should_suppress_timer_yield {
            emit_summary_events(
                &event_tx,
                next_seq.as_ref(),
                &Ok((
                    summary.clone(),
                    next_stored_values.clone(),
                    metadata.clone(),
                )),
            );
        }

        stored_values = next_stored_values;
        resume_state = advance_runtime_resume_state(resume_state, &summary, &metadata);
        wait_for_live_resume(
            &command_rx,
            timer_pending_resume_after_ms(summary.yield_value.as_ref()),
        )?;
    }
}

fn advance_runtime_resume_state(
    mut resume_state: runtime::ResumeState,
    summary: &super::response::ExecRunResult,
    metadata: &runtime::RunCellMetadata,
) -> runtime::ResumeState {
    resume_state
        .replayed_tool_calls
        .extend(metadata.newly_recorded_tool_calls.clone());
    resume_state.recorded_timer_calls = metadata.timer_calls.clone();
    resume_state.suppressed_text_calls = metadata.total_text_calls;
    resume_state.suppressed_notification_calls = metadata.total_notification_calls;
    if matches!(summary.yield_kind, Some(ExecYieldKind::Manual)) {
        resume_state.skipped_yields += 1;
    }
    resume_state
}

fn wait_for_live_resume(
    command_rx: &SharedCommandReceiver,
    resume_after_ms: Option<u64>,
) -> Result<(), crate::tools::ToolError> {
    match resume_after_ms {
        Some(delay_ms) if delay_ms > 0 => wait_for_driver_commands(command_rx, delay_ms),
        _ => drain_driver_commands(command_rx),
    }
}

fn wait_for_driver_commands(
    command_rx: &SharedCommandReceiver,
    delay_ms: u64,
) -> Result<(), crate::tools::ToolError> {
    let deadline = std::time::Instant::now() + Duration::from_millis(delay_ms);
    loop {
        let Some(timeout) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return Ok(());
        };
        match command_rx.lock().unwrap().recv_timeout(timeout) {
            Ok(CellCommand::Cancel { reason }) => {
                return Err(crate::tools::ToolError::ExecutionFailed(reason));
            }
            Ok(CellCommand::Drain(_)) | Ok(CellCommand::ToolResult { .. }) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return Ok(()),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(crate::tools::ToolError::ExecutionFailed(
                    "Code mode runtime command channel closed unexpectedly.".to_string(),
                ));
            }
        }
    }
}

fn drain_driver_commands(
    command_rx: &SharedCommandReceiver,
) -> Result<(), crate::tools::ToolError> {
    loop {
        match command_rx.lock().unwrap().try_recv() {
            Ok(CellCommand::Cancel { reason }) => {
                return Err(crate::tools::ToolError::ExecutionFailed(reason));
            }
            Ok(CellCommand::Drain(_)) | Ok(CellCommand::ToolResult { .. }) => continue,
            Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(()),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return Err(crate::tools::ToolError::ExecutionFailed(
                    "Code mode runtime command channel closed unexpectedly.".to_string(),
                ));
            }
        }
    }
}

fn count_user_visible_events(events: &[RuntimeEvent]) -> usize {
    events
        .iter()
        .filter(|event| is_user_visible_event(event))
        .count()
}

fn last_user_visible_event(events: &[RuntimeEvent]) -> Option<&RuntimeEvent> {
    events
        .iter()
        .rev()
        .find(|event| is_user_visible_event(event))
}

fn is_user_visible_event(event: &RuntimeEvent) -> bool {
    matches!(
        event,
        RuntimeEvent::Text { .. }
            | RuntimeEvent::Notification { .. }
            | RuntimeEvent::Yield { .. }
            | RuntimeEvent::Completed { .. }
            | RuntimeEvent::Failed { .. }
            | RuntimeEvent::Cancelled { .. }
    )
}

fn emit_summary_events(
    event_tx: &tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
    next_seq: &AtomicU64,
    result: &Result<RuntimeCellResult, crate::tools::ToolError>,
) {
    match result {
        Ok((summary, _, _)) => {
            if !summary.output_text.trim().is_empty() {
                let _ = event_tx.send(RuntimeEvent::Text {
                    seq: next_seq.fetch_add(1, Ordering::Relaxed) + 1,
                    chunk: summary.output_text.clone(),
                });
            }
            for message in &summary.notifications {
                let _ = event_tx.send(RuntimeEvent::Notification {
                    seq: next_seq.fetch_add(1, Ordering::Relaxed) + 1,
                    message: message.clone(),
                });
            }
            if summary.yielded {
                let _ = event_tx.send(RuntimeEvent::Yield {
                    seq: next_seq.fetch_add(1, Ordering::Relaxed) + 1,
                    kind: summary.yield_kind.clone().unwrap_or(ExecYieldKind::Manual),
                    value: summary.yield_value.clone(),
                    resume_after_ms: timer_pending_resume_after_ms(summary.yield_value.as_ref()),
                });
            } else {
                let _ = event_tx.send(RuntimeEvent::Completed {
                    seq: next_seq.fetch_add(1, Ordering::Relaxed) + 1,
                    return_value: summary.return_value.clone(),
                });
            }
        }
        Err(err) => {
            let message = err.to_string();
            let seq = next_seq.fetch_add(1, Ordering::Relaxed) + 1;
            let event = if message.contains("cancel") || message.contains("interrupted") {
                RuntimeEvent::Cancelled {
                    seq,
                    reason: message,
                }
            } else {
                RuntimeEvent::Failed {
                    seq,
                    error: message,
                }
            };
            let _ = event_tx.send(event);
        }
    }
}

fn resume_progress_from_events(events: &[RuntimeEvent]) -> CellResumeProgressDelta {
    let mut progress = CellResumeProgressDelta::default();

    for event in events {
        match event {
            RuntimeEvent::Text { .. } => {
                progress.suppressed_text_calls_delta += 1;
            }
            RuntimeEvent::Notification { .. } => {
                progress.suppressed_notification_calls_delta += 1;
            }
            RuntimeEvent::ToolCallRequested(_) => {
                progress.total_nested_tool_calls_delta += 1;
            }
            RuntimeEvent::Yield {
                kind: ExecYieldKind::Manual,
                ..
            } => {
                progress.skipped_yields_delta += 1;
            }
            _ => {}
        }
    }

    progress
}

fn recorded_tool_result_json(result: &Result<String, crate::tools::ToolError>) -> String {
    match result {
        Ok(result_json) => result_json.clone(),
        Err(err) => serde_json::json!({
            "__rustyClawToolError": err.to_string(),
        })
        .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn synthetic_driver(
        event_rx: tokio::sync::mpsc::UnboundedReceiver<RuntimeEvent>,
        command_tx: std::sync::mpsc::Sender<CellCommand>,
    ) -> CellDriver {
        CellDriver {
            event_rx,
            command_tx,
            worker: None,
            resume_progress: Arc::new(Mutex::new(CellResumeProgressDelta::default())),
        }
    }

    #[tokio::test]
    async fn test_driver_collects_ordered_events_for_completed_cells() {
        let mut driver = CellDriver::spawn(
            "cell_driver_1".to_string(),
            r#"
text("hello");
notify("done");
"#
            .to_string(),
            Vec::new(),
            HashMap::new(),
            runtime::ResumeState::default(),
        );
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        let completion = driver
            .drive_to_completion_with_events(&mut invoke_tool)
            .await
            .expect("driver completes");
        let (summary, _, _) = completion.runtime_result;

        assert!(!summary.yielded);
        assert_strictly_monotonic(&completion.events);
        assert!(matches!(
            completion.events.first(),
            Some(RuntimeEvent::Text { chunk, .. }) if chunk.trim() == "hello"
        ));
        assert!(completion.events.iter().any(
            |event| matches!(event, RuntimeEvent::Notification { message, .. } if message == "done")
        ));
        assert!(matches!(
            completion.events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
    }

    #[tokio::test]
    async fn test_driver_poll_now_returns_empty_batch_without_buffered_events() {
        let (_event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (command_tx, _command_rx) = std::sync::mpsc::channel();
        let mut driver = synthetic_driver(event_rx, command_tx);
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        let batch = driver
            .drain_event_batch_with_request(DrainRequest::poll_now(), &mut invoke_tool)
            .await
            .expect("poll-now returns an empty batch");

        assert_eq!(batch.request, DrainRequest::poll_now());
        assert!(batch.is_empty());
        assert!(batch.terminal_result.is_none());
        assert_eq!(batch.resume_progress, CellResumeProgressDelta::default());
    }

    #[tokio::test]
    async fn test_driver_poll_now_returns_progress_batch_for_buffered_visible_events() {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        event_tx
            .send(RuntimeEvent::Text {
                seq: 1,
                chunk: "hello".to_string(),
            })
            .expect("buffered text event is queued");
        let (command_tx, _command_rx) = std::sync::mpsc::channel();
        let mut driver = synthetic_driver(event_rx, command_tx);
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        let batch = driver
            .drain_event_batch_with_request(DrainRequest::poll_now(), &mut invoke_tool)
            .await
            .expect("poll-now returns a progress batch");

        assert_eq!(batch.request, DrainRequest::poll_now());
        assert!(batch.terminal_result.is_none());
        assert!(matches!(
            batch.events.as_slice(),
            [RuntimeEvent::Text { chunk, .. }] if chunk == "hello"
        ));
    }

    #[tokio::test]
    async fn test_driver_poll_now_returns_terminal_batch_for_buffered_completion() {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        event_tx
            .send(RuntimeEvent::WorkerCompleted(Ok((
                crate::code_mode::response::ExecRunResult {
                    cell_id: "cell_poll_terminal".to_string(),
                    output_text: "done".to_string(),
                    return_value: Some(serde_json::json!("ok")),
                    yield_value: None,
                    yielded: false,
                    yield_kind: None,
                    notifications: Vec::new(),
                    nested_tool_calls: 0,
                    truncated: false,
                },
                HashMap::new(),
                runtime::RunCellMetadata::default(),
            ))))
            .expect("buffered completion event is queued");
        let (command_tx, _command_rx) = std::sync::mpsc::channel();
        let mut driver = synthetic_driver(event_rx, command_tx);
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        let batch = driver
            .drain_event_batch_with_request(DrainRequest::poll_now(), &mut invoke_tool)
            .await
            .expect("poll-now returns a terminal batch");

        assert_eq!(batch.request, DrainRequest::poll_now());
        assert!(batch.terminal_result.is_some());
        assert!(batch.events.is_empty());
    }

    #[tokio::test]
    async fn test_driver_wait_with_refresh_slice_stops_after_deadline() {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (command_tx, _command_rx) = std::sync::mpsc::channel();
        let mut driver = synthetic_driver(event_rx, command_tx);
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        tokio::spawn(async move {
            event_tx
                .send(RuntimeEvent::Text {
                    seq: 1,
                    chunk: "first".to_string(),
                })
                .expect("first text is queued");
            tokio::time::sleep(Duration::from_millis(20)).await;
            event_tx
                .send(RuntimeEvent::Text {
                    seq: 2,
                    chunk: "second".to_string(),
                })
                .expect("second text is queued");
        });

        let sliced = driver
            .drain_event_batch_with_request(DrainRequest::for_wait(None, Some(5)), &mut invoke_tool)
            .await
            .expect("refresh-sliced wait returns partial progress");

        assert!(matches!(
            sliced.events.as_slice(),
            [RuntimeEvent::Text { chunk, .. }] if chunk == "first"
        ));
        assert!(sliced.terminal_result.is_none());

        let resumed = driver
            .drain_event_batch_with_request(DrainRequest::wait_for_next_event(), &mut invoke_tool)
            .await
            .expect("next wait drains the later event");

        assert!(matches!(
            resumed.events.as_slice(),
            [RuntimeEvent::Text { chunk, .. }] if chunk == "second"
        ));
        assert!(resumed.terminal_result.is_none());
    }

    #[test]
    fn test_driver_drain_batch_helpers_flag_empty_wait_batches() {
        let batch = DriverDrainBatch::empty(DrainRequest::poll_now());

        assert!(batch.requested_wait_for_event());
        assert!(batch.is_empty());
        assert_eq!(batch.request, DrainRequest::poll_now());
        assert_eq!(batch.resume_progress, CellResumeProgressDelta::default());
    }

    #[test]
    fn test_driver_progress_batch_infers_resume_progress_from_events() {
        let batch = DriverDrainBatch::progress(
            DrainRequest::poll_now(),
            vec![
                RuntimeEvent::Text {
                    seq: 1,
                    chunk: "hello".to_string(),
                },
                RuntimeEvent::Notification {
                    seq: 2,
                    message: "done".to_string(),
                },
                RuntimeEvent::ToolCallRequested(ToolCallRequest {
                    seq: 3,
                    request_id: 7,
                    tool_name: "read_file".to_string(),
                    args_json: "{}".to_string(),
                }),
                RuntimeEvent::Yield {
                    seq: 4,
                    kind: ExecYieldKind::Manual,
                    value: Some(serde_json::json!("pause")),
                    resume_after_ms: None,
                },
                RuntimeEvent::Yield {
                    seq: 5,
                    kind: ExecYieldKind::Timer,
                    value: None,
                    resume_after_ms: Some(25),
                },
            ],
        );

        assert_eq!(batch.resume_progress.suppressed_text_calls_delta, 1);
        assert_eq!(batch.resume_progress.suppressed_notification_calls_delta, 1);
        assert_eq!(batch.resume_progress.total_nested_tool_calls_delta, 1);
        assert_eq!(batch.resume_progress.skipped_yields_delta, 1);
        assert!(batch.resume_progress.replayed_tool_calls_delta.is_empty());
        assert!(batch.resume_progress.recorded_timer_calls.is_none());
    }

    fn assert_strictly_monotonic(events: &[RuntimeEvent]) {
        let mut previous = 0u64;
        for event in events {
            let seq = event
                .seq()
                .unwrap_or_else(|| unreachable!("internal worker event is not buffered"));
            assert!(
                seq > previous,
                "event sequence did not increase monotonically"
            );
            previous = seq;
        }
    }
}
