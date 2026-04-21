use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::protocol::{RuntimeEvent, RuntimeTerminalResult, ToolCallRequestEvent};
use super::runtime;

pub struct CellDriver {
    event_rx: tokio::sync::mpsc::UnboundedReceiver<RuntimeEvent>,
    /// Channel for sending tool results back to the worker thread.
    tool_result_tx: std::sync::mpsc::Sender<Result<String, crate::tools::ToolError>>,
    /// Channel for sending commands (like resume) to the worker thread.
    command_tx: std::sync::mpsc::Sender<crate::code_mode::protocol::CellCommand>,
    worker: Option<tokio::task::JoinHandle<()>>,
    cancel_flag: Arc<AtomicBool>,
    pending_events: VecDeque<RuntimeEvent>,
}

#[derive(Clone)]
pub struct CellDriverControl {
    tool_result_tx: std::sync::mpsc::Sender<Result<String, crate::tools::ToolError>>,
    command_tx: std::sync::mpsc::Sender<crate::code_mode::protocol::CellCommand>,
    cancel_flag: Arc<AtomicBool>,
}

impl CellDriverControl {
    pub fn request_cancel(&self, reason: &str) {
        self.cancel_flag.store(true, Ordering::Relaxed);
        let _ = self
            .tool_result_tx
            .send(Err(crate::tools::ToolError::ExecutionFailed(
                reason.to_string(),
            )));
        let _ = self
            .command_tx
            .send(crate::code_mode::protocol::CellCommand::Cancel {
                reason: reason.to_string(),
            });
    }
}

impl std::fmt::Debug for CellDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CellDriver")
            .field("worker", &self.worker)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct DriverEventBatch {
    pub events: Vec<RuntimeEvent>,
}

impl DriverEventBatch {
    pub fn new(events: Vec<RuntimeEvent>) -> Self {
        Self { events }
    }
}

#[derive(Debug)]
pub enum DriverBoundary {
    Progress,
    PendingTool(ToolCallRequestEvent),
    Terminal(RuntimeTerminalResult),
    Idle,
}

#[derive(Debug)]
pub struct DriverUpdate {
    pub batch: DriverEventBatch,
    pub boundary: DriverBoundary,
}

impl DriverUpdate {
    fn progress(events: Vec<RuntimeEvent>) -> Self {
        Self {
            batch: DriverEventBatch::new(events),
            boundary: DriverBoundary::Progress,
        }
    }

    fn pending_tool(events: Vec<RuntimeEvent>, request: ToolCallRequestEvent) -> Self {
        Self {
            batch: DriverEventBatch::new(events),
            boundary: DriverBoundary::PendingTool(request),
        }
    }

    fn terminal(events: Vec<RuntimeEvent>, terminal_result: RuntimeTerminalResult) -> Self {
        Self {
            batch: DriverEventBatch::new(events),
            boundary: DriverBoundary::Terminal(terminal_result),
        }
    }

    fn idle() -> Self {
        Self {
            batch: DriverEventBatch::new(Vec::new()),
            boundary: DriverBoundary::Idle,
        }
    }
}

impl CellDriver {
    /// Spawn a live cell driver.
    ///
    /// The worker thread uses a channel-based `invoke_tool` bridge: when JS
    /// calls `tools.X()`, the worker emits a `ToolCallRequested` event and
    /// blocks waiting for a result on `tool_result_rx`. The host update loop
    /// sees the `ToolCallRequested`, calls
    /// the caller's async `invoke_tool`, and sends the result back.
    pub fn spawn_live(
        code: String,
        visible_tools: Vec<String>,
        stored_values: HashMap<String, runtime::value::StoredValue>,
    ) -> Self {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        // Use a std::sync channel for tool results so the blocking worker can
        // call recv() without entering an async runtime.
        let (tool_result_tx, tool_result_rx) =
            std::sync::mpsc::channel::<Result<String, crate::tools::ToolError>>();

        let (command_tx, command_rx) =
            std::sync::mpsc::channel::<crate::code_mode::protocol::CellCommand>();

        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_flag_for_worker = cancel_flag.clone();

        let event_tx_captured = event_tx.clone();

        let worker = tokio::task::spawn_blocking(move || {
            let request = runtime::RunCellRequest {
                code,
                visible_tools,
                stored_values,
                command_rx,
                cancel_flag: cancel_flag_for_worker,
            };

            // Build a synchronous invoke_tool that simply blocks waiting for
            // the host update loop to fulfill the tool call. The runtime's __callTool
            // already emits the ToolCallRequested event via event_tx, so the
            // host update loop will see it, call the real invoke_tool, and send the
            // result back here.
            let invoke_tool = move |_tool_name: String,
                                    _args_json: String|
                  -> Result<String, crate::tools::ToolError> {
                // Block until the host update loop sends us the result via std channel
                tool_result_rx.recv().unwrap_or_else(|_| {
                    Err(crate::tools::ToolError::ExecutionFailed(
                        "Tool result channel closed".to_string(),
                    ))
                })
            };

            let result = runtime::run_cell(
                tokio::runtime::Handle::current(),
                request,
                invoke_tool,
                event_tx_captured.clone(),
            );

            let _ = event_tx_captured.send(RuntimeEvent::WorkerCompleted(
                result.map_err(|e| e.to_string()),
            ));
        });

        Self {
            event_rx,
            tool_result_tx,
            command_tx,
            worker: Some(worker),
            cancel_flag,
            pending_events: VecDeque::new(),
        }
    }

    pub fn request_cancel(&mut self, reason: &str) {
        self.control_handle().request_cancel(reason);
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
    }

    pub fn control_handle(&self) -> CellDriverControl {
        CellDriverControl {
            tool_result_tx: self.tool_result_tx.clone(),
            command_tx: self.command_tx.clone(),
            cancel_flag: self.cancel_flag.clone(),
        }
    }

    /// Collect the next driver update from the live worker until a visible
    /// batch, terminal result, pending nested tool request, or idle timeout is reached.
    pub async fn next_update(
        &mut self,
        idle_timeout: Option<Duration>,
    ) -> Result<DriverUpdate, crate::tools::ToolError> {
        let mut events = Vec::new();

        loop {
            while let Some(event) = self.pending_events.pop_front() {
                if let Some(batch) = self.classify_event(&mut events, event)? {
                    return Ok(batch);
                }
            }

            // Drain any buffered events first
            while let Ok(event) = self.event_rx.try_recv() {
                if let Some(outcome) = self.classify_event(&mut events, event)? {
                    return Ok(outcome);
                }
            }

            // Block for the next event, or return control to the host if we
            // hit the optional idle timeout while waiting for an auto-flush
            // deadline.
            let next_event = if let Some(timeout) = idle_timeout {
                tokio::select! {
                    event = self.event_rx.recv() => event,
                    _ = tokio::time::sleep(timeout) => return Ok(DriverUpdate::idle()),
                }
            } else {
                self.event_rx.recv().await
            };

            let Some(event) = next_event else {
                if self.cancel_flag.load(Ordering::Relaxed) {
                    return Err(crate::tools::ToolError::ExecutionFailed(
                        "Code mode cell execution was cancelled.".to_string(),
                    ));
                }
                return Err(crate::tools::ToolError::ExecutionFailed(
                    "Worker thread unexpectedly terminated.".to_string(),
                ));
            };

            if let Some(outcome) = self.classify_event(&mut events, event)? {
                return Ok(outcome);
            }
        }
    }

    pub fn complete_pending_tool_call(
        &mut self,
        request: &ToolCallRequestEvent,
        result: Result<String, crate::tools::ToolError>,
    ) -> Result<(), crate::tools::ToolError> {
        let ok = result.is_ok();
        let result_for_js = match result {
            Ok(raw) => Ok(crate::code_mode::runtime::value::normalize_tool_result_for_js(&raw)),
            Err(err) => Err(err),
        };

        self.tool_result_tx.send(result_for_js).map_err(|_| {
            crate::tools::ToolError::ExecutionFailed("Tool result channel closed".to_string())
        })?;
        self.pending_events
            .push_back(RuntimeEvent::ToolCallDone {
                seq: request.seq,
                request_id: request.request_id.clone(),
                ok,
            });
        Ok(())
    }

    fn classify_event(
        &self,
        events: &mut Vec<RuntimeEvent>,
        event: RuntimeEvent,
    ) -> Result<Option<DriverUpdate>, crate::tools::ToolError> {
        match event {
            RuntimeEvent::ToolCallRequested(req) => {
                events.push(RuntimeEvent::ToolCallRequested(req.clone()));
                Ok(Some(DriverUpdate::pending_tool(
                    std::mem::take(events),
                    req,
                )))
            }
            RuntimeEvent::WorkerCompleted(result) => match result {
                Ok(terminal_result) => Ok(Some(DriverUpdate::terminal(
                    std::mem::take(events),
                    terminal_result,
                ))),
                Err(err_msg) => Err(crate::tools::ToolError::ExecutionFailed(err_msg)),
            },
            RuntimeEvent::Flush { .. } | RuntimeEvent::WaitingForTimer { .. } => {
                events.push(event);
                Ok(Some(DriverUpdate::progress(std::mem::take(events))))
            }
            event => {
                events.push(event);
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_driver_cancels_infinite_loop_and_thread_exits() {
        let mut driver = CellDriver::spawn_live(
            "while(true) {}".to_string(),
            vec![],
            HashMap::new(),
        );

        // Allow it to start and enter the loop
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Set the cancel flag without aborting the join handle yet
        driver.cancel_flag.store(true, Ordering::Relaxed);

        // Take the worker handle and await it
        let worker_handle = driver.worker.take().expect("Should have worker handle");

        // Wait for the thread to exit. We wrap this in a timeout just in case it doesn't,
        // so the test doesn't hang forever.
        let join_result =
            tokio::time::timeout(std::time::Duration::from_secs(2), worker_handle).await;

        assert!(
            join_result.is_ok(),
            "The worker thread did not exit within the timeout after cancellation flag was set!"
        );

        // It joined successfully
        let result = join_result.unwrap();
        assert!(result.is_ok(), "The worker thread should finish gracefully (or with a JS exception that is caught and converted to an Error event)");
    }
}
