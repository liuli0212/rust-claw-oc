use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::protocol::{RuntimeEvent, RuntimeTerminalResult};
use super::runtime;

pub struct CellDriver {
    event_rx: tokio::sync::mpsc::UnboundedReceiver<RuntimeEvent>,
    control: CellDriverControl,
    worker: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Clone)]
pub struct CellDriverControl {
    /// Channel for interrupting timer waits in the worker thread.
    cancel_tx: std::sync::mpsc::Sender<String>,
    cancel_flag: Arc<AtomicBool>,
}

impl CellDriverControl {
    pub fn request_cancel(&self, reason: &str) {
        self.cancel_flag.store(true, Ordering::Release);
        let _ = self.cancel_tx.send(reason.to_string());
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
pub enum DriverBoundary {
    Progress,
    PendingTool,
    Terminal(RuntimeTerminalResult),
    Idle,
}

#[derive(Debug)]
pub struct DriverUpdate {
    pub events: Vec<RuntimeEvent>,
    pub boundary: DriverBoundary,
}

impl CellDriver {
    #[cfg(test)]
    fn spawn_live(code: String) -> Self {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let host = Arc::new(crate::code_mode::host::EventBridgeHost {
            visible_tools: Vec::new(),
            event_tx: event_tx.clone(),
        });
        Self::spawn_live_with_host(code, HashMap::new(), host, event_tx, event_rx, cancel_flag)
    }

    pub(crate) fn spawn_live_with_host(
        code: String,
        stored_values: HashMap<String, runtime::value::StoredValue>,
        host: Arc<dyn crate::code_mode::host::CellRuntimeHost>,
        event_tx: tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
        event_rx: tokio::sync::mpsc::UnboundedReceiver<RuntimeEvent>,
        cancel_flag: Arc<AtomicBool>,
    ) -> Self {
        let (cancel_tx, cancel_rx) = std::sync::mpsc::channel::<String>();

        let cancel_flag_for_worker = cancel_flag.clone();
        let event_tx_captured = event_tx.clone();

        // spawn_blocking runs on a dedicated OS thread outside the async runtime.
        // run_cell calls block_on internally so it can drive the QuickJS async runtime
        // synchronously — QuickJS is not Send and cannot be moved across await points.
        let worker = tokio::task::spawn_blocking(move || {
            let request = runtime::RunCellRequest {
                code,
                stored_values,
                host,
                cancel_rx,
                cancel_flag: cancel_flag_for_worker,
            };

            let result = runtime::run_cell(tokio::runtime::Handle::current(), request);

            let _ = event_tx_captured.send(RuntimeEvent::WorkerCompleted(
                result.map_err(|e| e.to_string()),
            ));
        });

        Self {
            event_rx,
            control: CellDriverControl {
                cancel_tx,
                cancel_flag,
            },
            worker: Some(worker),
        }
    }

    pub fn request_cancel(&mut self, reason: &str) {
        self.control.request_cancel(reason);
        if let Some(worker) = self.worker.take() {
            // abort() on a spawn_blocking handle only detaches the JoinHandle —
            // it does NOT terminate the OS thread. The cancel_flag interrupt handler
            // and timer cancel command are what actually stop cooperative runtime work.
            worker.abort();
        }
    }

    pub fn control_handle(&self) -> CellDriverControl {
        self.control.clone()
    }

    /// Collect the next driver update from the live worker until a visible
    /// batch, terminal result, pending nested tool request, or idle timeout is reached.
    pub async fn next_update(
        &mut self,
        idle_timeout: Option<Duration>,
    ) -> Result<DriverUpdate, crate::tools::ToolError> {
        let mut events = Vec::new();

        loop {
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
                    _ = tokio::time::sleep(timeout) => {
                        return Ok(DriverUpdate {
                            events: Vec::new(),
                            boundary: DriverBoundary::Idle,
                        });
                    },
                }
            } else {
                self.event_rx.recv().await
            };

            let Some(event) = next_event else {
                if self.control.cancel_flag.load(Ordering::Acquire) {
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

    fn classify_event(
        &self,
        events: &mut Vec<RuntimeEvent>,
        event: RuntimeEvent,
    ) -> Result<Option<DriverUpdate>, crate::tools::ToolError> {
        match event {
            RuntimeEvent::ToolCallRequested(req) => {
                events.push(RuntimeEvent::ToolCallRequested(req));
                Ok(Some(DriverUpdate {
                    events: std::mem::take(events),
                    boundary: DriverBoundary::PendingTool,
                }))
            }
            RuntimeEvent::WorkerCompleted(result) => match result {
                Ok(terminal_result) => Ok(Some(DriverUpdate {
                    events: std::mem::take(events),
                    boundary: DriverBoundary::Terminal(terminal_result),
                })),
                Err(err_msg) => Err(crate::tools::ToolError::ExecutionFailed(err_msg)),
            },
            RuntimeEvent::Flush { .. } | RuntimeEvent::WaitingForTimer { .. } => {
                events.push(event);
                Ok(Some(DriverUpdate {
                    events: std::mem::take(events),
                    boundary: DriverBoundary::Progress,
                }))
            }
            event => {
                events.push(event);
                Ok(None)
            }
        }
    }
}

impl Drop for CellDriver {
    fn drop(&mut self) {
        self.request_cancel("driver dropped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_driver_cancels_infinite_loop_and_thread_exits() {
        let mut driver = CellDriver::spawn_live("while(true) {}".to_string());

        // Allow it to start and enter the loop
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Set the cancel flag without aborting the join handle yet
        driver.control.cancel_flag.store(true, Ordering::Relaxed);

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
