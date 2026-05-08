use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rquickjs::async_with;
use rquickjs::function::Async;
use rquickjs::prelude::{Func, Promise};
use rquickjs::{AsyncContext, AsyncRuntime, Error, Function};
use serde::Deserialize;

use self::timers::TimerRegistry;
use self::value::StoredValue;
use super::protocol::{RuntimeTerminalResult, ToolCallRequest};

pub mod timers;
pub mod value;

const USER_CODE_MARKER: &str = "/*__RUSTY_CLAW_USER_CODE__*/";
const WRAPPER_SCRIPT_TEMPLATE: &str = include_str!("wrapper.js");

pub(crate) struct RunCellRequest {
    pub(crate) code: String,
    pub(crate) stored_values: HashMap<String, StoredValue>,
    pub(crate) host: Arc<dyn crate::code_mode::host::CellRuntimeHost>,
    pub(crate) cancel_rx: tokio::sync::watch::Receiver<Option<String>>,
    pub(crate) cancel_flag: Arc<std::sync::atomic::AtomicBool>,
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

pub(crate) fn run_cell(
    handle: tokio::runtime::Handle,
    request: RunCellRequest,
) -> Result<RuntimeTerminalResult, crate::tools::ToolError> {
    handle.block_on(async move {
        let runtime = AsyncRuntime::new()
            .map_err(|err| crate::tools::ToolError::ExecutionFailed(err.to_string()))?;

        let cancel_flag_clone = request.cancel_flag.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || {
            cancel_flag_clone.load(std::sync::atomic::Ordering::Acquire)
        }))).await;
        let context: AsyncContext = AsyncContext::full(&runtime)
            .await
            .map_err(|err| crate::tools::ToolError::ExecutionFailed(err.to_string()))?;

        let RunCellRequest {
            code,
            stored_values,
            host,
            cancel_rx,
            cancel_flag: _,
        } = request;

        let next_seq_for_script = Arc::new(std::sync::atomic::AtomicU64::new(0));

        let stored_values = Arc::new(Mutex::new(stored_values));
        let timers = Arc::new(TimerRegistry::new(Instant::now()));
        let visible_tools_json = serde_json::to_string(&host.visible_tool_names())
            .map_err(|err| crate::tools::ToolError::ExecutionFailed(err.to_string()))?;

        let next_seq_for_script_captured = next_seq_for_script.clone();
        let stored_values_for_script = stored_values.clone();
        let timers_for_script = timers.clone();
        let host_for_script = host.clone();

        let return_payload = async_with!(context => |ctx| {
            let globals = ctx.globals();

            let host_for_text = host_for_script.clone();
            let next_seq_for_text = next_seq_for_script_captured.clone();
            globals
                .set(
                    "__text".to_string(),
                    Func::from(move |text: Option<String>| -> rquickjs::Result<()> {
                        host_for_text.emit_event(crate::code_mode::protocol::RuntimeEvent::Text {
                            seq: next_seq_for_text.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1,
                            text: text.unwrap_or_default(),
                        });
                        Ok(())
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let host_for_notify = host_for_script.clone();
            let next_seq_for_notify = next_seq_for_script_captured.clone();
            globals
                .set(
                    "__notify".to_string(),
                    Func::from(move |message: Option<String>| -> rquickjs::Result<()> {
                        host_for_notify.emit_event(crate::code_mode::protocol::RuntimeEvent::Notification {
                            seq: next_seq_for_notify.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1,
                            message: message.unwrap_or_default(),
                        });
                        Ok(())
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let host_for_flush = host_for_script.clone();
            let next_seq_for_flush = next_seq_for_script_captured.clone();
            globals
                .set(
                    "__flush",
                    Func::from(move |value_json: Option<String>| -> rquickjs::Result<()> {
                        let value_json = value_json.unwrap_or_default();
                        let flush_value = if value_json.is_empty() || value_json == "null" {
                            None
                        } else {
                            let v = serde_json::from_str(&value_json)
                                .map_err(|err| Error::new_from_js_message("flush", "json", err.to_string()))?;
                            Some(v)
                        };
                        host_for_flush.emit_event(crate::code_mode::protocol::RuntimeEvent::Flush {
                            seq: next_seq_for_flush.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1,
                            value: flush_value,
                        });
                        Ok(())
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let store_ref = stored_values_for_script.clone();
            globals
                .set(
                    "__store".to_string(),
                    Func::from(move |key: Option<String>, value_json: Option<String>| -> rquickjs::Result<()> {
                        let key = key.unwrap_or_default();
                        let value_json = value_json.unwrap_or_else(|| "null".to_string());
                        let value = serde_json::from_str::<serde_json::Value>(&value_json).map_err(
                            |err| Error::new_from_js_message("string", "json", err.to_string()),
                        )?;
                        store_ref.lock().unwrap_or_else(|e| e.into_inner()).insert(key, value);
                        Ok(())
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let load_ref = stored_values_for_script.clone();
            globals
                .set(
                    "__load".to_string(),
                    Func::from(move |key: Option<String>| -> rquickjs::Result<Option<String>> {
                        Ok(load_ref
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .get(&key.unwrap_or_default())
                            .map(serde_json::Value::to_string))
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let host_for_tool = host_for_script.clone();
            let next_seq_for_tool = next_seq_for_script_captured.clone();
            // __callTool is a Promise bridge into the host. Runtime owns only
            // request construction; visibility, policy, and execution stay in Rust host code.
            let call_tool = Function::new(
                ctx.clone(),
                Async(move |tool_name: String, args_json: String| {
                    let seq = next_seq_for_tool.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    let request_id = format!("{}-{}", tool_name, seq);
                    let host = host_for_tool.clone();
                    let request = ToolCallRequest {
                        seq,
                        request_id,
                        tool_name: tool_name.clone(),
                        args_json: args_json.clone(),
                    };

                    async move {
                        match host.call_tool(request).await {
                            Ok(result_json) => result_json,
                            Err(err) => serde_json::json!({
                                "__rustyClawToolError": err.to_string()
                            })
                            .to_string(),
                        }
                    }
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

            let timers_ref = timers_for_script.clone();
            globals
                .set(
                    "__setTimeout",
                    Func::from(move |delay_ms: i32| -> rquickjs::Result<String> {
                        let delay_ms = u64::try_from(delay_ms).unwrap_or_default();
                        let registration = timers_ref.register_timeout(delay_ms);
                        serde_json::to_string(&registration).map_err(|err| {
                            Error::new_from_js_message("timer", "json", err.to_string())
                        })
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let timers_ref = timers_for_script.clone();
            globals
                .set(
                    "__clearTimeout",
                    Func::from(move |timer_id: Option<String>| -> rquickjs::Result<()> {
                        timers_ref.clear_timeout(&timer_id.unwrap_or_default());
                        Ok(())
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let timers_ref = timers_for_script.clone();
            globals
                .set(
                    "__markTimeoutComplete",
                    Func::from(move |timer_id: Option<String>| -> rquickjs::Result<()> {
                        timers_ref.mark_timeout_completed(&timer_id.unwrap_or_default());
                        Ok(())
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let timers_ref = timers_for_script.clone();
            let host_for_timer = host_for_script.clone();
            let next_seq_for_timer = next_seq_for_script_captured.clone();
            let cancel_rx_for_timer = cancel_rx.clone();
            let next_timer_event = Function::new(
                ctx.clone(),
                Async(move || {
                    let timers = timers_ref.clone();
                    let host = host_for_timer.clone();
                    let next_seq = next_seq_for_timer.clone();
                    let cancel_rx = cancel_rx_for_timer.clone();
                    async move {
                        next_timer_event_json(timers, host, next_seq, cancel_rx).await
                    }
                }),
            )
            .map_err(js_error_to_tool_error)?
            .with_name("__next_timer_event")
            .map_err(js_error_to_tool_error)?;
            globals
                .set("__next_timer_event", next_timer_event)
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

        let stored_values = stored_values.lock().unwrap_or_else(|e| e.into_inner()).clone();
        Ok(RuntimeTerminalResult {
            return_value,
            runtime_error,
            cancellation_reason,
            stored_values,
        })
    })
}

async fn next_timer_event_json(
    timers: Arc<TimerRegistry>,
    host: Arc<dyn crate::code_mode::host::CellRuntimeHost>,
    next_seq: Arc<std::sync::atomic::AtomicU64>,
    mut cancel_rx: tokio::sync::watch::Receiver<Option<String>>,
) -> String {
    loop {
        if let Some(reason) = cancel_rx.borrow().clone() {
            return serde_json::json!({
                "cancelled": true,
                "reason": reason,
            })
            .to_string();
        }

        let pending = timers.pending_state();
        if let Some(timer_id) = pending.due_timer_ids.first() {
            return serde_json::json!({
                "timer_id": timer_id,
            })
            .to_string();
        }

        if !pending.has_pending_timers() {
            return r#"{"idle":true}"#.to_string();
        }

        host.emit_event(crate::code_mode::protocol::RuntimeEvent::WaitingForTimer {
            seq: next_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1,
            resume_after_ms: pending.resume_after_ms,
        });

        let wait_duration =
            std::time::Duration::from_millis(pending.resume_after_ms.unwrap_or_default());
        tokio::select! {
            _ = tokio::time::sleep(wait_duration) => {}
            _ = timers.wait_for_change() => {}
            changed = cancel_rx.changed() => {
                if changed.is_err() {
                    return r#"{"disconnected":true}"#.to_string();
                }
            }
        }
    }
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
