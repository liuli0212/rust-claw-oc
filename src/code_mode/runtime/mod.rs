use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rquickjs::async_with;
use rquickjs::prelude::{Func, MutFn, Promise};
use rquickjs::{AsyncContext, AsyncRuntime, Error, Function};

use self::timers::RecordedTimerCall;
use self::value::StoredValue;
use super::response::ExecRunResult;
use super::runtime::globals::{LOAD_FN, NOTIFY_FN, STORE_FN, TEXT_FN};

pub mod callbacks;
pub mod globals;
pub mod timers;
pub mod value;

pub struct RunCellRequest {
    pub cell_id: String,
    pub code: String,
    pub visible_tools: Vec<String>,
    pub stored_values: HashMap<String, StoredValue>,
    pub command_rx: std::sync::mpsc::Receiver<crate::code_mode::protocol::CellCommand>,
    pub cancel_flag: Arc<std::sync::atomic::AtomicBool>,
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
        let observed_timer_calls = Arc::new(Mutex::new(0usize));
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
        let observed_timer_calls_for_script = observed_timer_calls.clone();
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
            let observed_timer_calls_ref = observed_timer_calls_for_script.clone();
            let on_timer_calls_updated_ref = on_timer_calls_updated_for_script.clone();
            let timer_clock_for_register = timer_clock_start.clone();
            globals
                .set(
                    "__setTimeout",
                    Func::from(move |delay_ms: i32| -> rquickjs::Result<String> {
                        let delay_ms = u64::try_from(delay_ms).unwrap_or_default();
                        let call_index = {
                            let mut observed = observed_timer_calls_ref.lock().unwrap();
                            let current = *observed;
                            *observed += 1;
                            current
                        };
                            let registration = {
                                let mut timer_calls = timer_calls_ref.lock().unwrap();
                                let registration = self::timers::register_timeout(
                                    &mut timer_calls,
                                    call_index,
                                    delay_ms,
                                    monotonic_elapsed_ms(timer_clock_for_register.as_ref()),
                                )
                                .map_err(|err| {
                                    Error::new_from_js_message("timer", "resume", err.to_string())
                            })?;
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

            let timer_calls_ref = timer_calls_for_script.clone();
            let timer_clock_for_due = timer_clock_start.clone();
            globals
                .set(
                    "__dueTimersJson",
                    Func::from(move || -> rquickjs::Result<String> {
                        let due_timers = self::timers::due_timers(
                            &timer_calls_ref.lock().unwrap(),
                            monotonic_elapsed_ms(timer_clock_for_due.as_ref()),
                        );
                        serde_json::to_string(&due_timers).map_err(|err| {
                            Error::new_from_js_message("timer", "json", err.to_string())
                        })
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            // Register __wait_for_timer: blocks the worker thread for the specified duration
            // while checking for CellCommand::Cancel from the driver. Used by the timer
            // yield loop to pause JS execution autonomously.
            let command_rx_for_resume = command_rx.clone();
            globals
                .set(
                    "__wait_for_timer",
                    Func::from(move |ms: f64| -> rquickjs::Result<String> {
                        let rx = command_rx_for_resume.lock().unwrap();
                        let wait_dur = std::time::Duration::from_millis(ms as u64);
                        match rx.recv_timeout(wait_dur) {
                            Ok(crate::code_mode::protocol::CellCommand::Cancel { reason }) => {
                                Ok(format!(r#"{{"yield":true,"cancel":true,"reason":"{}"}}"#, reason))
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                                Ok(r#"{"continue":true}"#.to_string())
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                                Ok(r#"{"yield":true}"#.to_string())
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

        let payload = serde_json::from_str::<serde_json::Value>(&return_payload)
            .map_err(|err| crate::tools::ToolError::ExecutionFailed(err.to_string()))?;
        if let Some(runtime_error) = payload.get("runtimeError").and_then(serde_json::Value::as_str)
        {
            return Err(crate::tools::ToolError::ExecutionFailed(
                runtime_error.to_string(),
            ));
        }
        let flushed = payload
            .get("yielded")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let return_value = payload
            .get("returnValue")
            .cloned()
            .filter(|value| !value.is_null());
        let flush_value = payload
            .get("yieldValue")
            .cloned()
            .filter(|value| !value.is_null());
        let runtime_error = payload
            .get("runtimeError")
            .and_then(serde_json::Value::as_str)
            .map(String::from);

        let nested_tool_calls = *nested_tool_count.lock().unwrap();
        let stored_values = stored_values.lock().unwrap().clone();
        let lifecycle = if runtime_error.is_some() {
            super::response::ExecLifecycle::Failed
        } else if flushed {
            super::response::ExecLifecycle::Running
        } else {
            super::response::ExecLifecycle::Completed
        };
        Ok((
            ExecRunResult {
                cell_id,
                output_text: String::new(),
                return_value,
                flush_value,
                lifecycle,
                progress_kind: None,
                flushed,
                waiting_on_timer_ms: None,
                notifications: Vec::new(),
                failure: runtime_error,
                cancellation: None,
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
    let mut script = String::new();
    script.push_str("(async () => {\n");
    script.push_str("const __allTools = JSON.parse(__allToolsJson);\n");
    script.push_str("const tools = {};\n");
    script.push_str("for (const toolName of __allTools) {\n");
    script.push_str(
        "  tools[toolName] = async (args) => { const result = JSON.parse(await __callTool(toolName, JSON.stringify(args === undefined ? null : args))); if (result && result.__rustyClawToolError) { throw String(result.__rustyClawToolError); } return result; };\n",
    );
    script.push_str("}\n");
    script.push_str("globalThis.text = (value) => __text(String(value));\n");
    script.push_str("globalThis.notify = (value) => __notify(String(value));\n");
    script.push_str(
        "globalThis.store = (key, value) => __store(String(key), JSON.stringify(value === undefined ? null : value));\n",
    );
    script.push_str(
        "globalThis.load = (key) => { const raw = __load(String(key)); return raw == null ? undefined : JSON.parse(raw); };\n",
    );
    script.push_str("globalThis.exit = (value) => { throw { __rustyClawExit: true, value }; };\n");
    script.push_str("globalThis.flush = (value) => {\n");
    script.push_str("__flush(JSON.stringify(value));\n");
    script.push_str("};\n");
    script.push_str("globalThis.__allTimeoutCallbacks = {};\n");
    script.push_str("globalThis.__dueTimeoutCallbacks = [];\n");
    script.push_str("globalThis.__totalTimersRegistered = 0;\n");
    script.push_str(
        "globalThis.setTimeout = (callback, delayMs = 0) => { globalThis.__totalTimersRegistered++; if (typeof callback !== 'function') { throw new Error('setTimeout() requires a function callback'); } const normalizedDelay = Math.max(0, Math.trunc(Number(delayMs ?? 0) || 0)); const registration = JSON.parse(__setTimeout(normalizedDelay)); globalThis.__allTimeoutCallbacks[registration.timer_id] = callback; if (registration.action === 'run') { globalThis.__dueTimeoutCallbacks.push({ id: registration.timer_id, callback }); } return registration.timer_id; };\n",
    );
    script.push_str(
        "globalThis.clearTimeout = (timerId) => { if (timerId == null) { return undefined; } const normalizedId = String(timerId); __clearTimeout(normalizedId); globalThis.__dueTimeoutCallbacks = globalThis.__dueTimeoutCallbacks.filter((item) => item.id !== normalizedId); return undefined; };\n",
    );
    script.push_str("let __result;\n");
    script.push_str("try {\n");
    script.push_str("  __result = await (async () => {\n");
    script.push_str(code);
    script.push_str("\n  })();\n");
    script.push_str("} catch (err) {\n");
    script.push_str(
        "  if (err && err.__rustyClawExit) { return JSON.stringify({ yielded: false, yieldKind: null, returnValue: err.value === undefined ? null : err.value, yieldValue: null }); }\n",
    );
    script.push_str(
        "  const __msg = (err && (err.message || err.name)) ? `${err.name || 'Error'}: ${err.message || ''}` : String(err);\n",
    );
    script.push_str("  const __stack = err && err.stack ? String(err.stack) : '';\n");
    script.push_str(
        "  return JSON.stringify({ yielded: false, yieldKind: null, runtimeError: __stack ? __msg + '\\n' + __stack : __msg });\n",
    );
    script.push_str("}\n");

    script.push_str("while (true) {\n");
    script.push_str("  const dueTimerIds = JSON.parse(__dueTimersJson());\n");
    script.push_str("  for (const id of dueTimerIds) {\n");
    script.push_str("    if (globalThis.__allTimeoutCallbacks[id] && !globalThis.__dueTimeoutCallbacks.some(c => c.id === id)) {\n");
    script.push_str("      globalThis.__dueTimeoutCallbacks.push({ id, callback: globalThis.__allTimeoutCallbacks[id] });\n");
    script.push_str("    }\n");
    script.push_str("  }\n");

    script.push_str("  if (globalThis.__dueTimeoutCallbacks.length > 0) {\n");
    script.push_str("    const __batch = globalThis.__dueTimeoutCallbacks.splice(0);\n");
    script.push_str("    for (const __timer of __batch) {\n");
    script.push_str("      await __timer.callback();\n");
    script.push_str("__markTimeoutComplete(__timer.id);\n");
    script.push_str("    }\n");
    script.push_str("    continue;\n");
    script.push_str("  }\n");

    script.push_str("  const __timerState = JSON.parse(__timerStateJson());\n");
    script.push_str("  const __unfinishedTimerIds = Object.keys(globalThis.__allTimeoutCallbacks).filter(id => !__timerState.completed_ids?.includes(id));\n");
    script.push_str("  if (__timerState.pending_timers > 0 || (__unfinishedTimerIds.length > 0 && globalThis.__totalTimersRegistered > 0)) {\n");
    script.push_str("    const __yieldObj = { reason: 'timer_pending', pending_timers: __timerState.pending_timers || 1, next_timer_id: __timerState.next_timer_id, resume_after_ms: __timerState.resume_after_ms || 100 };\n");
    script.push_str("__waiting_for_timer(__timerState.resume_after_ms || 100);\n");
    script.push_str(
        "    const resume = JSON.parse(__wait_for_timer(__timerState.resume_after_ms || 100));\n",
    );
    script.push_str("    if (resume && (resume.yield || resume.cancel)) {\n");
    script.push_str(
        "      return JSON.stringify({ yielded: true, yieldKind: 'timer', returnValue: null, yieldValue: __yieldObj });\n",
    );
    script.push_str("    }\n");
    script.push_str("    continue;\n");
    script.push_str("  }\n");
    script.push_str("  break;\n");
    script.push_str("}\n");
    script.push_str(
        "return JSON.stringify({ yielded: false, yieldKind: null, returnValue: __result === undefined ? null : __result, yieldValue: null });\n",
    );
    script.push_str("})()\n");
    script
}

fn js_error_to_tool_error(err: Error) -> crate::tools::ToolError {
    crate::tools::ToolError::ExecutionFailed(err.to_string())
}
