use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use super::executor::{
    is_code_mode_nested_tool, CodeModeNestedToolExecutor, CodeModeNestedToolExecutorConfig,
};
use crate::code_mode::response::{ExecLifecycle, ExecProgressKind};
use crate::context::FunctionCall;
use crate::core::extensions::ExecutionExtension;
use crate::core::{AgentOutput, ExecutionGuardState, ToolDispatchOutcome};
use crate::tools::code_mode::{ExecArgs, WaitArgs};
use crate::tools::protocol::StructuredToolOutput;
use crate::tools::Tool;
use crate::trace::{TraceActor, TraceBus, TraceContext, TraceSpanHandle, TraceStatus};

pub(crate) struct CodeModeDispatchConfig {
    pub(crate) current_tools: Vec<Arc<dyn Tool>>,
    pub(crate) extensions: Vec<Arc<dyn ExecutionExtension>>,
    pub(crate) service: super::service::CodeModeService,
    pub(crate) session_id: String,
    pub(crate) reply_to: String,
    pub(crate) remaining_steps: usize,
    pub(crate) session_deadline: Option<Instant>,
    pub(crate) iteration_trace_ctx: Option<TraceContext>,
    pub(crate) parent_span_id: Option<String>,
    pub(crate) trace_bus: Arc<TraceBus>,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) cancel_token: Arc<Notify>,
    pub(crate) output: Arc<dyn AgentOutput>,
    pub(crate) is_autopilot: bool,
    pub(crate) todos_path: PathBuf,
    pub(crate) execution_guard_state: Arc<std::sync::Mutex<ExecutionGuardState>>,
}

enum CodeModeInvocation {
    Exec(ExecArgs),
    Wait(WaitArgs),
}

pub(crate) async fn dispatch_tool_call(
    call: &FunctionCall,
    config: CodeModeDispatchConfig,
) -> ToolDispatchOutcome {
    let start = Instant::now();
    let invocation = match parse_invocation(call, start) {
        Ok(invocation) => invocation,
        Err(outcome) => return outcome,
    };

    let (source_length, requested_cell_id, wait_timeout_ms, auto_flush_ms) = match &invocation {
        CodeModeInvocation::Exec(parsed) => (
            parsed.code.chars().count(),
            None,
            None,
            parsed.auto_flush_ms,
        ),
        CodeModeInvocation::Wait(parsed) => {
            (0usize, parsed.cell_id.clone(), parsed.wait_timeout_ms, None)
        }
    };

    let visible_tools: Vec<String> = config
        .current_tools
        .iter()
        .map(|tool| tool.name())
        .filter(|name| is_code_mode_nested_tool(name))
        .collect();

    record_trace_event(
        config.trace_bus.as_ref(),
        config.iteration_trace_ctx.as_ref(),
        config.parent_span_id.clone(),
        TraceActor::Tool,
        "code_mode_exec_started",
        TraceStatus::Ok,
        Some(call.name.clone()),
        serde_json::json!({
            "tool_name": call.name,
            "outer_tool_call_id": call.id.clone(),
            "provider": config.provider.clone(),
            "model": config.model.clone(),
            "source_length": source_length,
            "requested_cell_id": requested_cell_id,
            "wait_timeout_ms": wait_timeout_ms,
            "auto_flush_ms": auto_flush_ms,
            "visible_nested_tools": visible_tools.len(),
            "args_preview": crate::context::AgentContext::truncate_chars(&call.args.to_string(), 500),
        }),
    );

    let is_wait = matches!(invocation, CodeModeInvocation::Wait(_));

    // Register the cancel future early so a notify_waiters() that fires
    // between the caller's last await and our select! is not lost.
    let cancel_notified = config.cancel_token.notified();
    tokio::pin!(cancel_notified);

    let exec_result = if is_wait {
        // wait is read-only: no hard timeout, no abort on cancel — just observe.
        tokio::select! {
            result = run_invocation(call, invocation, visible_tools, &config) => {
                result
            }
            _ = &mut cancel_notified => {
                Err(crate::tools::ToolError::Cancelled(
                    "Code mode wait interrupted by user.".to_string(),
                ))
            }
        }
    } else {
        tokio::select! {
            result = tokio::time::timeout(
                Duration::from_secs(90),
                run_invocation(call, invocation, visible_tools, &config)
            ) => {
                match result {
                    Ok(Ok(summary)) => Ok(summary),
                    Ok(Err(err)) => Err(err),
                    Err(_) => {
                        config
                            .service
                            .abort_active_cell(&config.session_id, "Code mode execution timed out.")
                            .await;
                        Err(crate::tools::ToolError::Timeout)
                    }
                }
            }
            _ = &mut cancel_notified => {
                config
                    .service
                    .abort_active_cell(&config.session_id, "Code mode execution interrupted by user.")
                    .await;
                Err(crate::tools::ToolError::Cancelled(
                    "Code mode execution interrupted by user.".to_string(),
                ))
            }
        }
    };

    match exec_result {
        Ok(summary) => {
            let (event_name, event_status) = if summary.flushed {
                ("code_mode_exec_flushed", TraceStatus::Ok)
            } else {
                match &summary.lifecycle {
                    ExecLifecycle::Running => ("code_mode_exec_running", TraceStatus::Ok),
                    ExecLifecycle::Completed => ("code_mode_exec_finished", TraceStatus::Ok),
                    ExecLifecycle::Failed => ("code_mode_exec_failed", TraceStatus::Error),
                    ExecLifecycle::Cancelled => {
                        ("code_mode_exec_terminated", TraceStatus::Cancelled)
                    }
                }
            };
            let termination_reason = if summary.flushed {
                match summary.progress_kind.as_ref() {
                    Some(&ExecProgressKind::ExplicitFlush) => "flush",
                    Some(&ExecProgressKind::AutoFlush) => "auto_flush",
                    None => "progress",
                }
            } else {
                match &summary.lifecycle {
                    ExecLifecycle::Running => "running",
                    ExecLifecycle::Completed => "completed",
                    ExecLifecycle::Failed => "failed",
                    ExecLifecycle::Cancelled => "cancelled",
                }
            };

            record_trace_event(
                config.trace_bus.as_ref(),
                config.iteration_trace_ctx.as_ref(),
                config.parent_span_id,
                TraceActor::Tool,
                event_name,
                event_status,
                Some(summary.cell_id.clone()),
                serde_json::json!({
                    "tool_name": call.name.clone(),
                    "outer_tool_call_id": call.id.clone(),
                    "provider": config.provider,
                    "model": config.model,
                    "cell_id": summary.cell_id.clone(),
                    "source_length": source_length,
                    "requested_cell_id": requested_cell_id,
                    "flushed": summary.flushed,
                    "progress_kind": summary.progress_kind.clone(),
                    "flush_value": summary.flush_value.clone(),
                    "lifecycle": summary.lifecycle.clone(),
                    "nested_tool_calls": summary.nested_tool_calls,
                    "output_size_chars": summary.output_text.chars().count(),
                    "termination_reason": termination_reason,
                    "truncated": summary.truncated,
                }),
            );

            let (ok, exit_code, is_error) = code_mode_tool_result_status(&summary);
            let rendered_output = summary.render_output();
            if !rendered_output.is_empty() {
                config.output.on_text(&rendered_output).await;
                config.output.on_text("\n").await;
            }

            let result = StructuredToolOutput::new(
                call.name.clone(),
                ok,
                rendered_output,
                exit_code,
                Some(start.elapsed().as_millis()),
                summary.truncated,
            )
            .with_payload_kind("code_mode_exec")
            .to_json_string()
            .unwrap_or_else(|err| format!("Tool error: {}", err));

            ToolDispatchOutcome {
                result,
                is_error,
                stopped: false,
            }
        }
        Err(err) => {
            let is_stopped = matches!(
                err,
                crate::tools::ToolError::Timeout | crate::tools::ToolError::Cancelled(_)
            );
            let (event_name, event_status, termination_reason) =
                match &err {
                    crate::tools::ToolError::Timeout => {
                        ("code_mode_exec_terminated", TraceStatus::TimedOut, "timeout")
                    }
                    crate::tools::ToolError::Cancelled(_) => {
                        ("code_mode_exec_terminated", TraceStatus::Cancelled, "cancelled")
                    }
                    _ => ("code_mode_exec_finished", TraceStatus::Error, "runtime_error"),
                };

            record_trace_event(
                config.trace_bus.as_ref(),
                config.iteration_trace_ctx.as_ref(),
                config.parent_span_id,
                TraceActor::Tool,
                event_name,
                event_status,
                Some(err.to_string()),
                serde_json::json!({
                    "tool_name": call.name.clone(),
                    "outer_tool_call_id": call.id.clone(),
                    "provider": config.provider,
                    "model": config.model,
                    "source_length": source_length,
                    "requested_cell_id": requested_cell_id,
                    "termination_reason": termination_reason,
                    "error": err.to_string(),
                }),
            );

            let result = StructuredToolOutput::new(
                call.name.clone(),
                false,
                format!("{} runtime failed: {}", call.name, err),
                Some(1),
                Some(start.elapsed().as_millis()),
                false,
            )
            .with_payload_kind("code_mode_exec")
            .to_json_string()
            .unwrap_or_else(|serialize_err| format!("Tool error: {}", serialize_err));

            ToolDispatchOutcome {
                result,
                is_error: true,
                stopped: is_stopped,
            }
        }
    }
}

async fn run_invocation(
    call: &FunctionCall,
    invocation: CodeModeInvocation,
    visible_tools: Vec<String>,
    config: &CodeModeDispatchConfig,
) -> Result<super::response::ExecRunResult, crate::tools::ToolError> {
    match invocation {
        CodeModeInvocation::Exec(parsed) => {
            let cell_span = config.iteration_trace_ctx.as_ref().map(|ctx| {
                let span_ctx = ctx.with_parent_span_id(config.parent_span_id.clone());
                config.trace_bus.start_span(
                    &span_ctx,
                    TraceActor::Tool,
                    "code_mode_cell_background",
                    serde_json::json!({
                        "session_id": config.session_id,
                        "outer_tool_call_id": call.id.clone(),
                    }),
                )
            });
            let cell_span_id = cell_span.as_ref().map(span_id_string);
            let nested_executor = Arc::new(tokio::sync::Mutex::new(
                CodeModeNestedToolExecutor::new(CodeModeNestedToolExecutorConfig {
                    current_tools: config.current_tools.clone(),
                    visible_tools: visible_tools.clone(),
                    extensions: config.extensions.clone(),
                    session_id: config.session_id.clone(),
                    reply_to: config.reply_to.clone(),
                    remaining_steps: config.remaining_steps,
                    session_deadline: config.session_deadline,
                    iteration_trace_ctx: config.iteration_trace_ctx.clone(),
                    parent_span_id: cell_span_id,
                    outer_tool_call_id: call.id.clone(),
                    trace_bus: config.trace_bus.clone(),
                    provider: config.provider.clone(),
                    model: config.model.clone(),
                    cancel_token: config.cancel_token.clone(),
                    is_autopilot: config.is_autopilot,
                    todos_path: config.todos_path.clone(),
                    execution_guard_state: config.execution_guard_state.clone(),
                }),
            ));

            let invoke_tool = move |tool_name: String, args_json: String| {
                let nested_executor = nested_executor.clone();
                async move {
                    let mut executor = nested_executor.lock().await;
                    let raw = executor.execute_json(tool_name, args_json).await?;
                    Ok(crate::code_mode::runtime::value::normalize_tool_result_for_js(&raw))
                }
            };

            config
                .service
                .execute(
                    &config.session_id,
                    &parsed.code,
                    parsed.auto_flush_ms,
                    visible_tools,
                    invoke_tool,
                    cell_span,
                )
                .await
        }
        CodeModeInvocation::Wait(parsed) => {
            config
                .service
                .wait_with_request(
                    &config.session_id,
                    parsed.cell_id.as_deref(),
                    parsed.wait_timeout_ms,
                )
                .await
        }
    }
}

fn parse_invocation(
    call: &FunctionCall,
    start: Instant,
) -> Result<CodeModeInvocation, ToolDispatchOutcome> {
    match call.name.as_str() {
        "exec" => serde_json::from_value::<ExecArgs>(call.args.clone())
            .map(CodeModeInvocation::Exec)
            .map_err(|err| invalid_argument_outcome("exec", err, start)),
        "wait" => serde_json::from_value::<WaitArgs>(call.args.clone())
            .map(CodeModeInvocation::Wait)
            .map_err(|err| invalid_argument_outcome("wait", err, start)),
        _ => Err(ToolDispatchOutcome {
            result: format!("Tool `{}` is not a code-mode entry tool.", call.name),
            is_error: true,
            stopped: false,
        }),
    }
}

fn invalid_argument_outcome(
    tool_name: &str,
    err: serde_json::Error,
    start: Instant,
) -> ToolDispatchOutcome {
    let result = StructuredToolOutput::new(
        tool_name,
        false,
        format!("Invalid {} arguments: {}", tool_name, err),
        Some(1),
        Some(start.elapsed().as_millis()),
        false,
    )
    .with_payload_kind("code_mode_exec")
    .to_json_string()
    .unwrap_or_else(|serialize_err| {
        format!(
            "Tool error: failed to serialize {} error envelope: {serialize_err}",
            tool_name
        )
    });

    ToolDispatchOutcome {
        result,
        is_error: true,
        stopped: false,
    }
}

fn code_mode_tool_result_status(
    summary: &super::response::ExecRunResult,
) -> (bool, Option<i32>, bool) {
    match summary.lifecycle {
        ExecLifecycle::Running | ExecLifecycle::Completed => (true, Some(0), false),
        ExecLifecycle::Failed => (false, Some(1), true),
        ExecLifecycle::Cancelled => (false, Some(130), true),
    }
}

fn record_trace_event(
    trace_bus: &TraceBus,
    trace_ctx: Option<&TraceContext>,
    parent_span_id: Option<String>,
    actor: TraceActor,
    name: &str,
    status: TraceStatus,
    summary: Option<String>,
    attrs: serde_json::Value,
) {
    if let Some(trace_ctx) = trace_ctx {
        let event_ctx = trace_ctx.with_parent_span_id(parent_span_id);
        trace_bus.record_event(&event_ctx, actor, name, status, summary, attrs);
    }
}

fn span_id_string(span: &TraceSpanHandle) -> String {
    span.span_id().to_string()
}
