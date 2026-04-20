use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rquickjs::async_with;
use rquickjs::prelude::{Func, MutFn, Promise};
use rquickjs::{AsyncContext, AsyncRuntime, Error, Function};
use serde::Deserialize;

use self::timers::RecordedTimerCall;
use self::value::StoredValue;
use super::response::ExecRunResult;
use super::runtime::globals::{LOAD_FN, NOTIFY_FN, STORE_FN, TEXT_FN};

pub mod callbacks;
pub mod globals;
pub mod timers;
pub mod value;

const USER_CODE_MARKER: &str = "/*__RUSTY_CLAW_USER_CODE__*/";
const WRAPPER_SCRIPT_TEMPLATE: &str = include_str!("wrapper.js");

pub struct RunCellRequest {
    pub cell_id: String,
    pub code: String,
    pub visible_tools: Vec<String>,
    pub stored_values: HashMap<String, StoredValue>,
    pub command_rx: std::sync::mpsc::Receiver<crate::code_mode::protocol::CellCommand>,
    pub cancel_flag: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug, Deserialize)]
struct RuntimeCompletionPayload {
    #[serde(rename = "returnValue")]
    return_value: Option<serde_json::Value>,
    #[serde(rename = "runtimeError")]
    runtime_error: Option<String>,
    #[serde(rename = "cancellationReason")]
    cancellation_reason: Option<String>,
}

pub fn run_cell<F>(
    handle: tokio::runtime::Handle,
    request: RunCellRequest,
    invoke_tool: F,
    on_timer_calls_updated: impl Fn(Vec<RecordedTimerCall>) + Send + Sync + 'static,
    event_tx: tokio::sync::mpsc::UnboundedSender<crate::code_mode::protocol::RuntimeEvent>,
) -> Result<(ExecRunResult, HashMap<String, StoredValue>), crate::tools::ToolError>
where
    F: FnMut(String, String) -> Result<String, crate::tools::ToolError> + Send + 'static,
{
    handle.block_on(async move {
        let runtime = AsyncRuntime::new()
            .map_err(|err| crate::tools::ToolError::ExecutionFailed(err.to_string()))?;

        let cancel_flag_clone = request.cancel_flag.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || {
            cancel_flag_clone.load(std::sync::atomic::Ordering::Relaxed)
        }))).await;
        let context = AsyncContext::full(&runtime)
            .await
            .map_err(|err| crate::tools::ToolError::ExecutionFailed(err.to_string()))?;

        let RunCellRequest {
            cell_id,
            code,
            visible_tools,
            stored_values,
            command_rx,
            cancel_flag: _,
        } = request;

        let event_tx_for_script = event_tx.clone();
        let next_seq_for_script = Arc::new(std::sync::atomic::AtomicU64::new(0));

        let stored_values = Arc::new(Mutex::new(stored_values));
        let nested_tool_count = Arc::new(Mutex::new(0usize));
        let timer_calls = Arc::new(Mutex::new(Vec::<RecordedTimerCall>::new()));
        let invoke_tool = Arc::new(Mutex::new(invoke_tool));
        let on_timer_calls_updated = Arc::new(on_timer_calls_updated);
        let visible_tools_json = serde_json::to_string(&visible_tools)
            .map_err(|err| crate::tools::ToolError::ExecutionFailed(err.to_string()))?;

        let command_rx = Arc::new(Mutex::new(command_rx));
        let timer_clock_start = Arc::new(Instant::now());

        let event_tx_for_script_captured = event_tx_for_script.clone();
        let next_seq_for_script_captured = next_seq_for_script.clone();
        let stored_values_for_script = stored_values.clone();
        let nested_tool_count_for_script = nested_tool_count.clone();
        let timer_calls_for_script = timer_calls.clone();
        let invoke_tool_for_script = invoke_tool.clone();
        let on_timer_calls_updated_for_script = on_timer_calls_updated.clone();

        let return_payload = async_with!(context => |ctx| {
            let globals = ctx.globals();

            let event_tx_for_text = event_tx_for_script_captured.clone();
            let next_seq_for_text = next_seq_for_script_captured.clone();
            globals
                .set(
                    format!("__{TEXT_FN}"),
                    Func::from(move |text: String| -> rquickjs::Result<()> {
                        let _ = event_tx_for_text.send(crate::code_mode::protocol::RuntimeEvent::Text {
                            seq: next_seq_for_text.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1,
                            text,
                        });
                        Ok(())
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let event_tx_for_notify = event_tx_for_script_captured.clone();
            let next_seq_for_notify = next_seq_for_script_captured.clone();
            globals
                .set(
                    format!("__{NOTIFY_FN}"),
                    Func::from(move |message: String| -> rquickjs::Result<()> {
                        let _ = event_tx_for_notify.send(crate::code_mode::protocol::RuntimeEvent::Notification {
                            seq: next_seq_for_notify.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1,
                            message,
                        });
                        Ok(())
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let event_tx_for_flush = event_tx_for_script_captured.clone();
            let next_seq_for_flush = next_seq_for_script_captured.clone();
            globals
                .set(
                    "__flush",
                    Func::from(move |value_json: String| -> rquickjs::Result<()> {
                        let flush_value = if value_json.is_empty() || value_json == "null" { None } else { Some(serde_json::from_str(&value_json).unwrap_or(serde_json::Value::Null)) };
                        let _ = event_tx_for_flush.send(crate::code_mode::protocol::RuntimeEvent::Flush {
                            seq: next_seq_for_flush.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1,
                            value: flush_value,
                        });
                        Ok(())
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let event_tx_for_timer_wait = event_tx_for_script_captured.clone();
            let next_seq_for_timer_wait = next_seq_for_script_captured.clone();
            globals
                .set(
                    "__waiting_for_timer",
                    Func::from(move |resume_after_ms: Option<u64>| -> rquickjs::Result<()> {
                        let _ = event_tx_for_timer_wait.send(crate::code_mode::protocol::RuntimeEvent::WaitingForTimer {
                            seq: next_seq_for_timer_wait.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1,
                            resume_after_ms,
                        });
                        Ok(())
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let store_ref = stored_values_for_script.clone();
            globals
                .set(
                    format!("__{STORE_FN}"),
                    Func::from(move |key: String, value_json: String| -> rquickjs::Result<()> {
                        let value = serde_json::from_str::<serde_json::Value>(&value_json).map_err(
                            |err| Error::new_from_js_message("string", "json", err.to_string()),
                        )?;
                        store_ref.lock().unwrap().insert(key, value);
                        Ok(())
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let load_ref = stored_values_for_script.clone();
            globals
                .set(
                    format!("__{LOAD_FN}"),
                    Func::from(move |key: String| -> rquickjs::Result<Option<String>> {
                        Ok(load_ref
                            .lock()
                            .unwrap()
                            .get(&key)
                            .map(serde_json::Value::to_string))
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let event_tx_for_tool = event_tx_for_script_captured.clone();
            let next_seq_for_tool = next_seq_for_script_captured.clone();
            let call_tool_ref = nested_tool_count_for_script.clone();
            let invoke_tool_ref = invoke_tool_for_script.clone();
            let call_tool = Function::new(
                ctx.clone(),
                MutFn::from(move |tool_name: String, args_json: String| -> rquickjs::Result<String> {
                    *call_tool_ref.lock().unwrap() += 1;
                    let seq = next_seq_for_tool.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    let request_id = format!("{}-{}", tool_name, seq);
                    let _ = event_tx_for_tool.send(crate::code_mode::protocol::RuntimeEvent::ToolCallRequested(
                        crate::code_mode::protocol::ToolCallRequestEvent {
                            seq,
                            request_id: request_id.clone(),
                            tool_name: tool_name.clone(),
                            args_json: args_json.clone(),
                        }
                    ));

                    let tool_result = {
                        let mut lock = invoke_tool_ref.lock().unwrap();
                        (lock)(tool_name.clone(), args_json.clone())
                    };

                    let result_json = match tool_result {
                        Ok(result_json) => result_json,
                        Err(err) => {
                            let tool_err: crate::tools::ToolError = err;
                            serde_json::json!({
                                "__rustyClawToolError": tool_err.to_string()
                            })
                            .to_string()
                        }
                    };

                    Ok(result_json)
                }),
            )
            .map_err(js_error_to_tool_error)?
            .with_name("__callTool")
            .map_err(js_error_to_tool_error)?;
            globals
                .set("__callTool", call_tool)
                .map_err(js_error_to_tool_error)?;

            globals
                .set("__allToolsJson", visible_tools_json.clone())
                .map_err(js_error_to_tool_error)?;

            let timer_calls_ref = timer_calls_for_script.clone();
            let on_timer_calls_updated_ref = on_timer_calls_updated_for_script.clone();
            let timer_clock_for_register = timer_clock_start.clone();
            globals
                .set(
                    "__setTimeout",
                    Func::from(move |delay_ms: i32| -> rquickjs::Result<String> {
                        let delay_ms = u64::try_from(delay_ms).unwrap_or_default();
                        let registration = {
                            let mut timer_calls = timer_calls_ref.lock().unwrap();
                            let registration = self::timers::register_timeout(
                                &mut timer_calls,
                                delay_ms,
                                monotonic_elapsed_ms(timer_clock_for_register.as_ref()),
                            );
                            on_timer_calls_updated_ref(timer_calls.clone());
                            registration
                        };
                        serde_json::to_string(&registration).map_err(|err| {
                            Error::new_from_js_message("timer", "json", err.to_string())
                        })
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let timer_calls_ref = timer_calls_for_script.clone();
            let on_timer_calls_updated_ref = on_timer_calls_updated_for_script.clone();
            globals
                .set(
                    "__clearTimeout",
                    Func::from(move |timer_id: String| -> rquickjs::Result<()> {
                        let mut timer_calls = timer_calls_ref.lock().unwrap();
                        self::timers::clear_timeout(&mut timer_calls, &timer_id);
                        on_timer_calls_updated_ref(timer_calls.clone());
                        Ok(())
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let timer_calls_ref = timer_calls_for_script.clone();
            let on_timer_calls_updated_ref = on_timer_calls_updated_for_script.clone();
            globals
                .set(
                    "__markTimeoutComplete",
                    Func::from(move |timer_id: String| -> rquickjs::Result<()> {
                        let mut timer_calls = timer_calls_ref.lock().unwrap();
                        self::timers::mark_timeout_completed(&mut timer_calls, &timer_id);
                        on_timer_calls_updated_ref(timer_calls.clone());
                        Ok(())
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let timer_calls_ref = timer_calls_for_script.clone();
            let timer_clock_for_pending = timer_clock_start.clone();
            globals
                .set(
                    "__timerStateJson",
                    Func::from(move || -> rquickjs::Result<String> {
                        let pending = self::timers::pending_timer_state(
                            &timer_calls_ref.lock().unwrap(),
                            monotonic_elapsed_ms(timer_clock_for_pending.as_ref()),
                        );
                        serde_json::to_string(&pending).map_err(|err| {
                            Error::new_from_js_message("timer", "json", err.to_string())
                        })
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            // Register __wait_for_timer: blocks the worker thread for the specified duration
            // while checking for CellCommand::Cancel from the driver.
            let command_rx_for_resume = command_rx.clone();
            globals
                .set(
                    "__wait_for_timer",
                    Func::from(move |ms: f64| -> rquickjs::Result<String> {
                        let rx = command_rx_for_resume.lock().unwrap();
                        let wait_dur = std::time::Duration::from_millis(ms as u64);
                        match rx.recv_timeout(wait_dur) {
                            Ok(crate::code_mode::protocol::CellCommand::Cancel { reason }) => {
                                Ok(serde_json::json!({
                                    "cancelled": true,
                                    "reason": reason,
                                })
                                .to_string())
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                                Ok(r#"{"continued":true}"#.to_string())
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                                Ok(r#"{"disconnected":true}"#.to_string())
                            }
                        }
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let promise: Promise = ctx
                .eval(build_wrapper_script(&code))
                .map_err(js_error_to_tool_error)?;
            promise
                .into_future::<String>()
                .await
                .map_err(js_error_to_tool_error)
        })
        .await?;

        let payload = serde_json::from_str::<RuntimeCompletionPayload>(&return_payload)
            .map_err(|err| crate::tools::ToolError::ExecutionFailed(err.to_string()))?;
        let return_value = payload.return_value.filter(|value| !value.is_null());
        let runtime_error = payload.runtime_error;
        let cancellation_reason = payload.cancellation_reason;

        let nested_tool_calls = *nested_tool_count.lock().unwrap();
        let stored_values = stored_values.lock().unwrap().clone();
        let lifecycle = if runtime_error.is_some() {
            super::response::ExecLifecycle::Failed
        } else if cancellation_reason.is_some() {
            super::response::ExecLifecycle::Cancelled
        } else {
            super::response::ExecLifecycle::Completed
        };
        Ok((
            ExecRunResult {
                cell_id,
                output_text: String::new(),
                return_value,
                flush_value: None,
                lifecycle,
                progress_kind: None,
                flushed: false,
                waiting_on_tool_request_id: None,
                waiting_on_timer_ms: None,
                notifications: Vec::new(),
                failure: runtime_error,
                cancellation: cancellation_reason,
                nested_tool_calls,
                truncated: false,
            },
            stored_values,
        ))
    })
}

fn monotonic_elapsed_ms(start: &Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn build_wrapper_script(code: &str) -> String {
    let (prefix, suffix) = WRAPPER_SCRIPT_TEMPLATE
        .split_once(USER_CODE_MARKER)
        .expect("wrapper.js is missing the user-code marker");

    let mut script = String::with_capacity(prefix.len() + code.len() + suffix.len());
    script.push_str(prefix);
    script.push_str(code);
    script.push_str(suffix);
    script
}

fn js_error_to_tool_error(err: Error) -> crate::tools::ToolError {
    crate::tools::ToolError::ExecutionFailed(err.to_string())
}
