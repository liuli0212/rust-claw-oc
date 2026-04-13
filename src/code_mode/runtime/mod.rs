use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rquickjs::async_with;
use rquickjs::prelude::{Func, MutFn, Promise};
use rquickjs::{AsyncContext, AsyncRuntime, Error, Function};

use self::callbacks::RecordedToolCall;
use self::timers::RecordedTimerCall;
use self::value::StoredValue;
use super::response::{ExecRunResult, ExecYieldKind};
use super::runtime::globals::{LOAD_FN, NOTIFY_FN, STORE_FN, TEXT_FN};

pub mod callbacks;
pub mod globals;
pub mod timers;
pub mod value;

const MAX_OUTPUT_CHARS: usize = 12_000;

#[derive(Debug, Clone, Default)]
pub struct ResumeState {
    pub replayed_tool_calls: Vec<RecordedToolCall>,
    pub recorded_timer_calls: Vec<RecordedTimerCall>,
    pub skipped_yields: usize,
    pub suppressed_text_calls: usize,
    pub suppressed_notification_calls: usize,
}

#[derive(Debug, Clone, Default)]
pub struct RunCellMetadata {
    pub total_text_calls: usize,
    pub total_notification_calls: usize,
    pub newly_recorded_tool_calls: Vec<RecordedToolCall>,
    pub timer_calls: Vec<RecordedTimerCall>,
}

#[derive(Debug, Default)]
struct OutputBuffer {
    text: String,
    truncated: bool,
    total_calls: usize,
    suppressed_calls: usize,
}

impl OutputBuffer {
    fn with_suppressed_calls(suppressed_calls: usize) -> Self {
        Self {
            text: String::new(),
            truncated: false,
            total_calls: 0,
            suppressed_calls,
        }
    }

    fn push_line(&mut self, value: &str) {
        self.total_calls += 1;
        if self.total_calls <= self.suppressed_calls {
            return;
        }

        if self.truncated {
            return;
        }

        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(value);

        if self.text.chars().count() > MAX_OUTPUT_CHARS {
            self.text = crate::context::AgentContext::truncate_chars(&self.text, MAX_OUTPUT_CHARS);
            self.truncated = true;
        }
    }
}

#[derive(Debug, Default)]
struct NotificationBuffer {
    items: Vec<String>,
    total_calls: usize,
    suppressed_calls: usize,
}

impl NotificationBuffer {
    fn with_suppressed_calls(suppressed_calls: usize) -> Self {
        Self {
            items: Vec::new(),
            total_calls: 0,
            suppressed_calls,
        }
    }

    fn push(&mut self, value: String) {
        self.total_calls += 1;
        if self.total_calls <= self.suppressed_calls {
            return;
        }
        self.items.push(value);
    }
}

pub fn run_cell<F>(
    handle: tokio::runtime::Handle,
    cell_id: String,
    code: String,
    visible_tools: Vec<String>,
    stored_values: HashMap<String, StoredValue>,
    resume_state: ResumeState,
    invoke_tool: F,
) -> Result<(ExecRunResult, HashMap<String, StoredValue>, RunCellMetadata), crate::tools::ToolError>
where
    F: FnMut(String, String) -> Result<String, crate::tools::ToolError> + Send + 'static,
{
    handle.block_on(async move {
        let runtime = AsyncRuntime::new()
            .map_err(|err| crate::tools::ToolError::ExecutionFailed(err.to_string()))?;
        let context = AsyncContext::full(&runtime)
            .await
            .map_err(|err| crate::tools::ToolError::ExecutionFailed(err.to_string()))?;

        let ResumeState {
            replayed_tool_calls,
            recorded_timer_calls,
            skipped_yields,
            suppressed_text_calls,
            suppressed_notification_calls,
        } = resume_state;

        let output = Arc::new(Mutex::new(OutputBuffer::with_suppressed_calls(
            suppressed_text_calls,
        )));
        let notifications = Arc::new(Mutex::new(NotificationBuffer::with_suppressed_calls(
            suppressed_notification_calls,
        )));
        let stored_values = Arc::new(Mutex::new(stored_values));
        let nested_tool_count = Arc::new(Mutex::new(0usize));
        let replayed_tool_calls = Arc::new(replayed_tool_calls);
        let observed_tool_calls = Arc::new(Mutex::new(0usize));
        let newly_recorded_tool_calls = Arc::new(Mutex::new(Vec::<RecordedToolCall>::new()));
        let timer_calls = Arc::new(Mutex::new(recorded_timer_calls));
        let observed_timer_calls = Arc::new(Mutex::new(0usize));
        let invoke_tool = Arc::new(Mutex::new(invoke_tool));
        let visible_tools_json = serde_json::to_string(&visible_tools)
            .map_err(|err| crate::tools::ToolError::ExecutionFailed(err.to_string()))?;
        let skipped_yields = i32::try_from(skipped_yields).unwrap_or(i32::MAX);
        let output_for_script = output.clone();
        let notifications_for_script = notifications.clone();
        let stored_values_for_script = stored_values.clone();
        let nested_tool_count_for_script = nested_tool_count.clone();
        let replayed_tool_calls_for_script = replayed_tool_calls.clone();
        let observed_tool_calls_for_script = observed_tool_calls.clone();
        let newly_recorded_tool_calls_for_script = newly_recorded_tool_calls.clone();
        let timer_calls_for_script = timer_calls.clone();
        let observed_timer_calls_for_script = observed_timer_calls.clone();
        let invoke_tool_for_script = invoke_tool.clone();

        let return_payload = async_with!(context => |ctx| {
            let globals = ctx.globals();

            let output_ref = output_for_script.clone();
            globals
                .set("__allToolsJson", visible_tools_json.clone())
                .map_err(js_error_to_tool_error)?;
            globals
                .set(
                    format!("__{TEXT_FN}"),
                    Func::from(move |value: String| -> rquickjs::Result<()> {
                        output_ref.lock().unwrap().push_line(&value);
                        Ok(())
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let notifications_ref = notifications_for_script.clone();
            globals
                .set(
                    format!("__{NOTIFY_FN}"),
                    Func::from(move |value: String| -> rquickjs::Result<()> {
                        notifications_ref.lock().unwrap().push(value);
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

            let call_tool_ref = nested_tool_count_for_script.clone();
            let replayed_tool_calls_ref = replayed_tool_calls_for_script.clone();
            let observed_tool_calls_ref = observed_tool_calls_for_script.clone();
            let newly_recorded_tool_calls_ref = newly_recorded_tool_calls_for_script.clone();
            let invoke_tool_ref = invoke_tool_for_script.clone();
            let call_tool = Function::new(
                ctx.clone(),
                MutFn::from(move |tool_name: String, args_json: String| -> rquickjs::Result<String> {
                    let call_index = {
                        let mut observed = observed_tool_calls_ref.lock().unwrap();
                        let current = *observed;
                        *observed += 1;
                        current
                    };

                    if let Some(recorded) = replayed_tool_calls_ref.get(call_index) {
                        if recorded.tool_name != tool_name || recorded.args_json != args_json {
                            return Err(Error::new_from_js_message(
                                "tool_call",
                                "replay",
                                format!(
                                    "Code mode resume diverged at nested tool call {}: expected {}({}), got {}({})",
                                    call_index + 1,
                                    recorded.tool_name,
                                    recorded.args_json,
                                    tool_name,
                                    args_json,
                                ),
                            ));
                        }

                        return Ok(recorded.result_json.clone());
                    }

                    *call_tool_ref.lock().unwrap() += 1;
                    let result_json = {
                        let mut invoke_tool = invoke_tool_ref.lock().unwrap();
                        (*invoke_tool)(tool_name.clone(), args_json.clone())
                    }
                    .map_err(|err| {
                        Error::new_from_js_message("tool_call", "promise", err.to_string())
                    })?;
                    newly_recorded_tool_calls_ref
                        .lock()
                        .unwrap()
                        .push(RecordedToolCall {
                            tool_name,
                            args_json,
                            result_json: result_json.clone(),
                        });
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
                .set("__skippedYields", skipped_yields)
                .map_err(js_error_to_tool_error)?;
            let timer_calls_ref = timer_calls_for_script.clone();
            let observed_timer_calls_ref = observed_timer_calls_for_script.clone();
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
                        let registration = self::timers::register_timeout(
                            &mut timer_calls_ref.lock().unwrap(),
                            call_index,
                            delay_ms,
                            crate::trace::unix_ms_now(),
                        )
                        .map_err(|err| {
                            Error::new_from_js_message("timer", "resume", err.to_string())
                        })?;
                        serde_json::to_string(&registration).map_err(|err| {
                            Error::new_from_js_message("timer", "json", err.to_string())
                        })
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let timer_calls_ref = timer_calls_for_script.clone();
            globals
                .set(
                    "__clearTimeout",
                    Func::from(move |timer_id: String| -> rquickjs::Result<()> {
                        self::timers::clear_timeout(&mut timer_calls_ref.lock().unwrap(), &timer_id);
                        Ok(())
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let timer_calls_ref = timer_calls_for_script.clone();
            globals
                .set(
                    "__markTimeoutComplete",
                    Func::from(move |timer_id: String| -> rquickjs::Result<()> {
                        self::timers::mark_timeout_completed(
                            &mut timer_calls_ref.lock().unwrap(),
                            &timer_id,
                        );
                        Ok(())
                    }),
                )
                .map_err(js_error_to_tool_error)?;

            let timer_calls_ref = timer_calls_for_script.clone();
            globals
                .set(
                    "__timerStateJson",
                    Func::from(move || -> rquickjs::Result<String> {
                        let pending = self::timers::pending_timer_state(
                            &timer_calls_ref.lock().unwrap(),
                            crate::trace::unix_ms_now(),
                        );
                        serde_json::to_string(&pending).map_err(|err| {
                            Error::new_from_js_message("timer", "json", err.to_string())
                        })
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
        let yielded = payload
            .get("yielded")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let return_value = payload
            .get("returnValue")
            .cloned()
            .filter(|value| !value.is_null());
        let yield_value = payload
            .get("yieldValue")
            .cloned()
            .filter(|value| !value.is_null());
        let yield_kind = payload
            .get("yieldKind")
            .and_then(serde_json::Value::as_str)
            .and_then(|value| match value {
                "manual" => Some(ExecYieldKind::Manual),
                "timer" => Some(ExecYieldKind::Timer),
                _ => None,
            });
        let output_guard = output.lock().unwrap();
        let notifications_guard = notifications.lock().unwrap();
        let nested_tool_calls = *nested_tool_count.lock().unwrap();
        let stored_values = stored_values.lock().unwrap().clone();
        let newly_recorded_tool_calls = newly_recorded_tool_calls.lock().unwrap().clone();
        let timer_calls = timer_calls.lock().unwrap().clone();
        Ok((
            ExecRunResult {
                cell_id,
                output_text: output_guard.text.clone(),
                return_value,
                yield_value,
                yielded,
                yield_kind,
                notifications: notifications_guard.items.clone(),
                nested_tool_calls,
                truncated: output_guard.truncated,
            },
            stored_values,
            RunCellMetadata {
                total_text_calls: output_guard.total_calls,
                total_notification_calls: notifications_guard.total_calls,
                newly_recorded_tool_calls,
                timer_calls,
            },
        ))
    })
}

fn build_wrapper_script(code: &str) -> String {
    let mut script = String::new();
    script.push_str("(async () => {\n");
    script.push_str("const __allTools = JSON.parse(__allToolsJson);\n");
    script.push_str("globalThis.ALL_TOOLS = Object.freeze(__allTools.slice());\n");
    script.push_str(
        "globalThis.tools = new Proxy({}, { get(_target, prop) { if (typeof prop !== 'string') { return undefined; } if (!ALL_TOOLS.includes(prop)) { throw new Error(`Tool not available in code mode: ${String(prop)}`); } return async (args = {}) => JSON.parse(await __callTool(prop, JSON.stringify(args ?? {}))); } });\n",
    );
    script.push_str("globalThis.text = (value) => __text(String(value));\n");
    script.push_str("globalThis.notify = (value) => __notify(String(value));\n");
    script.push_str(
        "globalThis.store = (key, value) => __store(String(key), JSON.stringify(value === undefined ? null : value));\n",
    );
    script.push_str(
        "globalThis.load = (key) => { const raw = __load(String(key)); return raw == null ? undefined : JSON.parse(raw); };\n",
    );
    script.push_str("globalThis.exit = (value) => { throw { __rustyClawExit: true, value }; };\n");
    script.push_str("globalThis.__yieldCount = 0;\n");
    script.push_str("globalThis.__dueTimeoutCallbacks = [];\n");
    script.push_str(
        "globalThis.yield_control = (value) => { if (globalThis.__yieldCount < __skippedYields) { globalThis.__yieldCount += 1; return undefined; } globalThis.__yieldCount += 1; throw { __rustyClawYield: true, value: value === undefined ? null : value }; };\n",
    );
    script.push_str(
        "globalThis.setTimeout = (callback, delayMs = 0) => { if (typeof callback !== 'function') { throw new Error('setTimeout() requires a function callback'); } const normalizedDelay = Math.max(0, Math.trunc(Number(delayMs ?? 0) || 0)); const registration = JSON.parse(__setTimeout(normalizedDelay)); if (registration.action === 'run') { globalThis.__dueTimeoutCallbacks.push({ id: registration.timer_id, callback }); } return registration.timer_id; };\n",
    );
    script.push_str(
        "globalThis.clearTimeout = (timerId) => { if (timerId == null) { return undefined; } const normalizedId = String(timerId); __clearTimeout(normalizedId); globalThis.__dueTimeoutCallbacks = globalThis.__dueTimeoutCallbacks.filter((item) => item.id !== normalizedId); return undefined; };\n",
    );
    script.push_str("try {\n");
    script.push_str("const __result = await (async () => {\n");
    script.push_str(code);
    script.push_str("\n})();\n");
    script.push_str("while (globalThis.__dueTimeoutCallbacks.length > 0) {\n");
    script.push_str("const __batch = globalThis.__dueTimeoutCallbacks.splice(0);\n");
    script.push_str("for (const __timer of __batch) {\n");
    script.push_str("await __timer.callback();\n");
    script.push_str("__markTimeoutComplete(__timer.id);\n");
    script.push_str("}\n");
    script.push_str("}\n");
    script.push_str("const __timerState = JSON.parse(__timerStateJson());\n");
    script.push_str("if (__timerState.pending_timers > 0) {\n");
    script.push_str(
        "return JSON.stringify({ yielded: true, yieldKind: 'timer', returnValue: null, yieldValue: { reason: 'timer_pending', pending_timers: __timerState.pending_timers, next_timer_id: __timerState.next_timer_id, resume_after_ms: __timerState.resume_after_ms } });\n",
    );
    script.push_str("}\n");
    script.push_str(
        "return JSON.stringify({ yielded: false, yieldKind: null, returnValue: __result === undefined ? null : __result, yieldValue: null });\n",
    );
    script.push_str("} catch (err) {\n");
    script.push_str(
        "if (err && err.__rustyClawExit) { return JSON.stringify({ yielded: false, yieldKind: null, returnValue: err.value === undefined ? null : err.value, yieldValue: null }); }\n",
    );
    script.push_str(
        "if (err && err.__rustyClawYield) { return JSON.stringify({ yielded: true, yieldKind: 'manual', returnValue: null, yieldValue: err.value === undefined ? null : err.value }); }\n",
    );
    script.push_str("throw err;\n");
    script.push_str("}\n");
    script.push_str("})()");
    script
}

fn js_error_to_tool_error(err: Error) -> crate::tools::ToolError {
    crate::tools::ToolError::ExecutionFailed(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_run_cell_supports_text_and_exit() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let (result, stored_values, metadata) = run_cell(
            runtime.handle().clone(),
            "cell_runtime_1".to_string(),
            r#"
text("hello");
exit({ done: true });
"#
            .to_string(),
            Vec::new(),
            HashMap::new(),
            ResumeState::default(),
            |_tool, _args| Ok("\"unused\"".to_string()),
        )
        .expect("runtime cell executes");

        assert_eq!(result.output_text.trim(), "hello");
        assert_eq!(
            result.return_value,
            Some(serde_json::json!({ "done": true }))
        );
        assert!(!result.yielded);
        assert_eq!(metadata.total_text_calls, 1);
        assert!(stored_values.is_empty());
    }

    #[test]
    fn test_run_cell_supports_timer_driven_resume() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let code = r#"
text("before");
setTimeout(async () => {
  text("later");
}, 20);
text("after");
"#;

        let (first, stored_values, metadata) = run_cell(
            runtime.handle().clone(),
            "cell_runtime_2".to_string(),
            code.to_string(),
            Vec::new(),
            HashMap::new(),
            ResumeState::default(),
            |_tool, _args| Ok("\"unused\"".to_string()),
        )
        .expect("runtime cell yields on timer");

        assert!(first.yielded);
        assert_eq!(first.yield_kind, Some(ExecYieldKind::Timer));
        assert_eq!(first.output_text.trim(), "before\nafter");
        assert_eq!(metadata.total_text_calls, 2);

        std::thread::sleep(Duration::from_millis(30));

        let (resumed, _, resumed_metadata) = run_cell(
            runtime.handle().clone(),
            "cell_runtime_2".to_string(),
            code.to_string(),
            Vec::new(),
            stored_values,
            ResumeState {
                recorded_timer_calls: metadata.timer_calls,
                suppressed_text_calls: metadata.total_text_calls,
                ..ResumeState::default()
            },
            |_tool, _args| Ok("\"unused\"".to_string()),
        )
        .expect("runtime cell resumes after timer fires");

        assert!(!resumed.yielded);
        assert_eq!(resumed.output_text.trim(), "later");
        assert_eq!(resumed_metadata.total_text_calls, 3);
    }
}
