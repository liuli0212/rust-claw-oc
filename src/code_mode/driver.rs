use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant as TokioInstant;

use super::protocol::{
    CellCommand, DrainRequest, RuntimeCellResult, RuntimeEvent, ToolCallRequest,
};
use super::runtime;

#[derive(Debug)]
pub struct CellDriver {
    event_rx: tokio::sync::mpsc::UnboundedReceiver<RuntimeEvent>,
    command_tx: std::sync::mpsc::Sender<CellCommand>,
    worker: Option<tokio::task::JoinHandle<()>>,
    
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
    }

impl DriverDrainBatch {
    pub fn empty(request: DrainRequest) -> Self {
        Self::progress(request, Vec::new())
    }

    pub fn progress(request: DrainRequest, events: Vec<RuntimeEvent>) -> Self {
                Self {
            request,
            terminal_result: None,
            events,
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
                    }
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
    
}

impl CellDriver {
    pub fn spawn(
        cell_id: String,
        code: String,
        visible_tools: Vec<String>,
        stored_values: HashMap<String, serde_json::Value>,
    ) -> Self {
        Self::spawn_with_mode(
            cell_id,
            code,
            visible_tools,
            stored_values,
            DriverMode::OneShot,
        )
    }

    pub fn spawn_live(
        cell_id: String,
        code: String,
        visible_tools: Vec<String>,
        stored_values: HashMap<String, serde_json::Value>,
        suppress_initial_timer_yield: bool,
    ) -> Self {
        Self::spawn_with_mode(
            cell_id,
            code,
            visible_tools,
            stored_values,
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
        _mode: DriverMode,
    ) -> Self {
        let handle = tokio::runtime::Handle::current();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<RuntimeEvent>();
        let (command_tx, command_rx) = std::sync::mpsc::channel::<CellCommand>();
        let command_rx = Arc::new(Mutex::new(command_rx));
                let worker = tokio::task::spawn_blocking(move || {
            let runtime_event_tx = event_tx.clone();
            let completion_event_tx = event_tx;
            let worker_state = WorkerRuntimeState {
                command_rx,
                event_tx: runtime_event_tx.clone(),
                next_request_id: Arc::new(AtomicU64::new(0)),
                next_seq: Arc::new(AtomicU64::new(0)),
                
            };
            let next_seq = worker_state.next_seq.clone();
            let result = run_worker_iteration(
                handle,
                runtime::RunCellRequest {
                    cell_id,
                    code,
                    visible_tools,
                    stored_values,
                },
                worker_state.clone(),
            );
            
            match &result {
                Ok((summary, _)) => {
                    let _ = runtime_event_tx.send(RuntimeEvent::Completed {
                        seq: next_seq.fetch_add(1, Ordering::Relaxed) + 1,
                        return_value: summary.return_value.clone(),
                    });
                }
                Err(err) => {
                    let message = err.to_string();
                    let seq = next_seq.fetch_add(1, Ordering::Relaxed) + 1;
                    let event = if message.contains("cancel") || message.contains("interrupted") {
                        RuntimeEvent::Cancelled { seq, reason: message }
                    } else {
                        RuntimeEvent::Failed { seq, error: message }
                    };
                    let _ = runtime_event_tx.send(event);
                }
            }
            let _ = completion_event_tx.send(RuntimeEvent::WorkerCompleted(result));
        });

        Self {
            event_rx,
            command_tx,
            worker: Some(worker),
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

    fn finish_progress_batch(
        &self,
        request: DrainRequest,
        events: Vec<RuntimeEvent>,
    ) -> DriverDrainBatch {
        if events.is_empty() {
            DriverDrainBatch::empty(request)
        } else {
            DriverDrainBatch::progress(request, events)
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
            } = worker_state;
    let tool_event_tx = event_tx.clone();
    let next_seq_for_tool = next_seq.clone();
    runtime::run_cell(
        handle,
        request,
        event_tx,
        next_seq,
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
        |_timer_calls| {},
    )
}


#[allow(dead_code)]
fn wait_for_live_resume(
    command_rx: &SharedCommandReceiver,
    resume_after_ms: Option<u64>,
) -> Result<(), crate::tools::ToolError> {
    match resume_after_ms {
        Some(delay_ms) if delay_ms > 0 => wait_for_driver_commands(command_rx, delay_ms),
        _ => drain_driver_commands(command_rx),
    }
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
fn recorded_tool_result_json(result: &Result<String, crate::tools::ToolError>) -> String {
    match result {
        Ok(result_json) => result_json.clone(),
        Err(err) => serde_json::json!({
            "__rustyClawToolError": err.to_string(),
        })
        .to_string(),
    }
}

