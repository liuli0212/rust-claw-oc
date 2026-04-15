use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant as TokioInstant;

use super::protocol::{DrainRequest, RuntimeCellResult, RuntimeEvent};
use super::runtime;

pub struct CellDriver {
    pub cell_id: String,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<RuntimeEvent>,
    /// Channel for sending tool results back to the worker thread.
    tool_result_tx: std::sync::mpsc::Sender<Result<String, crate::tools::ToolError>>,
    /// Channel for sending commands (like resume) to the worker thread.
    command_tx: std::sync::mpsc::Sender<crate::code_mode::protocol::CellCommand>,
    #[allow(dead_code)] // Kept to hold the JoinHandle alive
    worker: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for CellDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CellDriver")
            .field("worker", &self.worker)
            .finish_non_exhaustive()
    }
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

impl CellDriver {
    /// Spawn a live cell driver.
    ///
    /// The worker thread uses a channel-based `invoke_tool` bridge: when JS
    /// calls `tools.X()`, the worker emits a `ToolCallRequested` event and
    /// blocks waiting for a result on `tool_result_rx`. The drain loop in
    /// `drain_event_batch_with_request` sees the `ToolCallRequested`, calls
    /// the caller's async `invoke_tool`, and sends the result back.
    pub fn spawn_live(
        cell_id: String,
        code: String,
        visible_tools: Vec<String>,
        stored_values: HashMap<String, runtime::value::StoredValue>,
        _suppress_initial_timer_flush: bool,
    ) -> Self {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        // Use a std::sync channel for tool results so the blocking worker can
        // call recv() without entering an async runtime.
        let (tool_result_tx, tool_result_rx) =
            std::sync::mpsc::channel::<Result<String, crate::tools::ToolError>>();

        let (command_tx, command_rx) =
            std::sync::mpsc::channel::<crate::code_mode::protocol::CellCommand>();

        let event_tx_captured = event_tx.clone();
        let next_seq = Arc::new(AtomicU64::new(0));

        let next_seq_for_worker = next_seq.clone();

        let cell_id_for_worker = cell_id.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let request = runtime::RunCellRequest {
                cell_id: cell_id_for_worker,
                code,
                visible_tools,
                stored_values,
                command_rx,
            };

            // Build a synchronous invoke_tool that simply blocks waiting for
            // the drain loop to fulfill the tool call. The runtime's __callTool
            // already emits the ToolCallRequested event via event_tx, so the
            // drain loop will see it, call the real invoke_tool, and send the
            // result back here.
            let invoke_tool = move |_tool_name: String,
                                    _args_json: String|
                  -> Result<String, crate::tools::ToolError> {
                // Block until the drain loop sends us the result via std channel
                tool_result_rx.recv().unwrap_or_else(|_| {
                    Err(crate::tools::ToolError::ExecutionFailed(
                        "Tool result channel closed".to_string(),
                    ))
                })
            };

            let event_tx_for_timer = event_tx_captured.clone();
            let next_seq_for_timer = next_seq_for_worker.clone();

            let result = runtime::run_cell(
                tokio::runtime::Handle::current(),
                request,
                invoke_tool,
                move |timer_calls| {
                    let _ = event_tx_for_timer.send(RuntimeEvent::TimerRegistrationChanged {
                        seq: next_seq_for_timer.fetch_add(1, Ordering::Relaxed) + 1,
                        timer_calls,
                    });
                },
                event_tx_captured.clone(),
            );

            let _ = event_tx_captured.send(RuntimeEvent::WorkerCompleted(
                result.map_err(|e| e.to_string()),
            ));
        });

        Self {
            cell_id,
            event_rx,
            tool_result_tx,
            command_tx,
            worker: Some(worker),
        }
    }

    /// Drain events from the live worker, fulfilling nested tool calls via
    /// `invoke_tool` when `ToolCallRequested` events are seen.
    pub async fn drain_event_batch_with_request<F, Fut>(
        &mut self,
        request: DrainRequest,
        invoke_tool: &mut F,
        send_resume: bool,
    ) -> Result<DriverDrainBatch, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        if send_resume {
            // Signal the worker to resume if it's waiting, passing the current request
            let _ = self
                .command_tx
                .send(crate::code_mode::protocol::CellCommand::Drain(
                    request.clone(),
                ));
        }

        let mut events = Vec::new();
        let refresh_deadline = request
            .refresh_slice_ms
            .map(|refresh_ms| TokioInstant::now() + Duration::from_millis(refresh_ms));
        let mut saw_visible_event = false;

        loop {
            // Drain any buffered events first
            while let Ok(event) = self.event_rx.try_recv() {
                if let Some(batch) = self
                    .classify_event(
                        request,
                        &mut events,
                        event,
                        invoke_tool,
                        &mut saw_visible_event,
                    )
                    .await?
                {
                    return Ok(batch);
                }
            }

            // Determine how to wait for the next event.
            // If we have a refresh_deadline (from refresh_slice_ms), we wait
            // for more events until the deadline, then return whatever we have.
            // If no refresh_deadline and no wait_timeout: we are in
            // "to_completion" mode — keep blocking until a terminal/flush
            // event arrives (those are handled in classify_event which
            // returns Some(batch)).
            let next_event = if let Some(deadline) = refresh_deadline {
                if saw_visible_event && TokioInstant::now() >= deadline {
                    return Ok(DriverDrainBatch::progress(request, events));
                }
                match tokio::time::timeout_at(deadline, self.event_rx.recv()).await {
                    Ok(event) => event,
                    Err(_) => return Ok(DriverDrainBatch::progress(request, events)),
                }
            } else if let Some(wait_timeout_ms) = request.wait_timeout_ms {
                match tokio::time::timeout(
                    Duration::from_millis(wait_timeout_ms),
                    self.event_rx.recv(),
                )
                .await
                {
                    Ok(event) => event,
                    Err(_) => return Ok(DriverDrainBatch::progress(request, events)),
                }
            } else {
                // to_completion mode: block indefinitely for the next event.
                // classify_event will return the batch when a terminal or
                // flush event arrives.
                self.event_rx.recv().await
            };

            let Some(event) = next_event else {
                return Ok(DriverDrainBatch::progress(request, events));
            };

            if let Some(batch) = self
                .classify_event(
                    request,
                    &mut events,
                    event,
                    invoke_tool,
                    &mut saw_visible_event,
                )
                .await?
            {
                return Ok(batch);
            }
        }
    }

    /// Classify a single event. If it's a tool request, fulfill it via
    /// invoke_tool and send the result back to the worker.
    async fn classify_event<F, Fut>(
        &self,
        request: DrainRequest,
        events: &mut Vec<RuntimeEvent>,
        event: RuntimeEvent,
        invoke_tool: &mut F,
        saw_visible_event: &mut bool,
    ) -> Result<Option<DriverDrainBatch>, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        match event {
            RuntimeEvent::ToolCallRequested(ref req) => {
                let tool_name = req.tool_name.clone();
                let args_json = req.args_json.clone();
                let req_seq = req.seq;
                let req_id = req.request_id.clone();
                events.push(event);

                // Execute the nested tool call
                let result = invoke_tool(tool_name, args_json).await;
                let ok = result.is_ok();

                // Normalize for JS
                let result_for_js = match result {
                    Ok(raw) => {
                        Ok(crate::code_mode::runtime::value::normalize_tool_result_for_js(&raw))
                    }
                    Err(e) => Err(e),
                };

                // Send result back to the worker thread
                let _ = self.tool_result_tx.send(result_for_js);

                // Record the resolution event
                events.push(RuntimeEvent::ToolCallResolved {
                    seq: req_seq,
                    request_id: req_id,
                    ok,
                });

                Ok(None)
            }
            RuntimeEvent::WorkerCompleted(result) => match result {
                Ok(cell_result) => Ok(Some(DriverDrainBatch::terminal(
                    request,
                    cell_result,
                    std::mem::take(events),
                ))),
                Err(err_msg) => Err(crate::tools::ToolError::ExecutionFailed(err_msg)),
            },
            RuntimeEvent::Flush { .. } | RuntimeEvent::WaitingForTimer { .. } => {
                events.push(event);
                Ok(Some(DriverDrainBatch::progress(
                    request,
                    std::mem::take(events),
                )))
            }
            event => {
                if event.is_visible_to_drain() {
                    *saw_visible_event = true;
                }
                events.push(event);
                Ok(None)
            }
        }
    }
}
