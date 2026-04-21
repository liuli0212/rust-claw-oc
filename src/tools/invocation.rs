use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::context::AgentContext;
use crate::core::extensions::ExecutionExtension;
use crate::core::{ExecutionGuardSignal, ExecutionGuardState};
use crate::tools::{Tool, ToolContext};
use crate::trace::{TraceActor, TraceBus, TraceContext, TraceStatus};

#[derive(Debug, Clone)]
pub(crate) struct ToolInvocationOutcome {
    pub(crate) result: String,
    pub(crate) is_error: bool,
    pub(crate) stopped: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ToolInvocationEndNames {
    pub(crate) success: &'static str,
    pub(crate) error: &'static str,
    pub(crate) timeout: &'static str,
    pub(crate) cancelled: &'static str,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolInvocationSpanConfig {
    pub(crate) actor: TraceActor,
    pub(crate) start_name: &'static str,
    pub(crate) start_attrs: Value,
    pub(crate) end_names: ToolInvocationEndNames,
    pub(crate) end_attrs: Value,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolInvocationRequest<'a> {
    pub(crate) tool_name: &'a str,
    pub(crate) args: Value,
    pub(crate) timeout: Duration,
    pub(crate) trace_ctx: Option<TraceContext>,
    pub(crate) context_parent_span_id: Option<String>,
    pub(crate) span: Option<ToolInvocationSpanConfig>,
}

pub(crate) struct ToolInvokerConfig {
    pub(crate) current_tools: Vec<Arc<dyn Tool>>,
    pub(crate) visible_tools: Vec<String>,
    pub(crate) extensions: Vec<Arc<dyn ExecutionExtension>>,
    pub(crate) session_id: String,
    pub(crate) reply_to: String,
    pub(crate) remaining_steps: usize,
    pub(crate) session_deadline: Option<Instant>,
    pub(crate) trace_bus: Arc<TraceBus>,
    pub(crate) cancel_token: CancellationToken,
    pub(crate) is_autopilot: bool,
    pub(crate) todos_path: PathBuf,
    pub(crate) execution_guard_state: Arc<std::sync::Mutex<ExecutionGuardState>>,
}

pub(crate) struct ToolInvoker {
    current_tools: Vec<Arc<dyn Tool>>,
    visible_tools: Vec<String>,
    extensions: Vec<Arc<dyn ExecutionExtension>>,
    session_id: String,
    reply_to: String,
    remaining_steps: usize,
    session_deadline: Option<Instant>,
    trace_bus: Arc<TraceBus>,
    cancel_token: CancellationToken,
    is_autopilot: bool,
    todos_path: PathBuf,
    execution_guard_state: Arc<std::sync::Mutex<ExecutionGuardState>>,
}

impl ToolInvoker {
    pub(crate) fn new(config: ToolInvokerConfig) -> Self {
        Self {
            current_tools: config.current_tools,
            visible_tools: config.visible_tools,
            extensions: config.extensions,
            session_id: config.session_id,
            reply_to: config.reply_to,
            remaining_steps: config.remaining_steps,
            session_deadline: config.session_deadline,
            trace_bus: config.trace_bus,
            cancel_token: config.cancel_token,
            is_autopilot: config.is_autopilot,
            todos_path: config.todos_path,
            execution_guard_state: config.execution_guard_state,
        }
    }

    pub(crate) fn decrement_remaining_steps(&mut self) {
        self.remaining_steps = self.remaining_steps.saturating_sub(1);
    }

    pub(crate) fn visible_tools(&self) -> &[String] {
        &self.visible_tools
    }

    pub(crate) fn autopilot_denial_for_call(
        &self,
        call_name: &str,
        call_args: &Value,
    ) -> Option<String> {
        if !self.is_autopilot {
            return None;
        }

        let tool_has_effects = self
            .current_tools
            .iter()
            .find(|tool| tool.name() == call_name)
            .map(|tool| tool.has_side_effects())
            .unwrap_or(true);
        if !tool_has_effects {
            return None;
        }

        if self.todos_path.exists() {
            return None;
        }

        let is_creating_todos = match call_name {
            "write_file" => call_args
                .get("path")
                .and_then(|p| p.as_str())
                .and_then(|p| std::path::Path::new(p).file_name())
                .and_then(|n| n.to_str())
                .map(|n| n.eq_ignore_ascii_case("TODOS.md"))
                .unwrap_or(false),
            "execute_bash" => call_args
                .get("command")
                .and_then(|c| c.as_str())
                .map(|cmd| cmd.contains("TODOS.md"))
                .unwrap_or(false),
            _ => false,
        };
        if is_creating_todos {
            return None;
        }

        Some(
            "[System Error] Action Denied. Autopilot 模式下必须先创建并规划 TODOS.md。".to_string(),
        )
    }

    pub(crate) fn record_action_outcome(
        &self,
        call_name: &str,
        call_args: &Value,
        is_error: bool,
    ) -> Option<ExecutionGuardSignal> {
        if !self.is_autopilot {
            return None;
        }

        let mut guard_state = self
            .execution_guard_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard_state.record_action_outcome(call_name, call_args, is_error)
    }

    pub(crate) async fn prepare_tool_context(
        &self,
        trace_ctx: Option<&TraceContext>,
        parent_span_id: Option<String>,
    ) -> ToolContext {
        let mut ctx = ToolContext::new(self.session_id.clone(), self.reply_to.clone());
        ctx.visible_tools = self.visible_tools.clone();
        ctx.skill_budget.remaining_steps = Some(self.remaining_steps);
        ctx.skill_budget.remaining_timeout_sec = self.remaining_session_timeout_sec();
        if let Some(trace_ctx) = trace_ctx {
            ctx.trace = Some(crate::tools::protocol::ToolTraceContext {
                trace_id: trace_ctx.trace_id.clone(),
                run_id: trace_ctx.run_id.clone(),
                root_session_id: trace_ctx.root_session_id.clone(),
                task_id: trace_ctx.task_id.clone(),
                turn_id: trace_ctx.turn_id.clone(),
                iteration: trace_ctx.iteration,
                parent_span_id,
            });
        }
        for ext in &self.extensions {
            ctx = ext.enrich_tool_context(ctx).await;
        }
        ctx
    }

    pub(crate) async fn invoke(&self, request: ToolInvocationRequest<'_>) -> ToolInvocationOutcome {
        let Some(tool) = self
            .current_tools
            .iter()
            .find(|tool| tool.name() == request.tool_name)
        else {
            return ToolInvocationOutcome {
                result: format!("Tool not found: {}", request.tool_name),
                is_error: true,
                stopped: false,
            };
        };

        let mut invocation_span = request.span.as_ref().and_then(|span_config| {
            request.trace_ctx.as_ref().map(|trace_ctx| {
                let span_ctx =
                    trace_ctx.with_parent_span_id(request.context_parent_span_id.clone());
                (
                    self.trace_bus.start_span(
                        &span_ctx,
                        span_config.actor.clone(),
                        span_config.start_name,
                        span_config.start_attrs.clone(),
                    ),
                    span_config.clone(),
                )
            })
        });

        let tool_ctx_parent_span_id = invocation_span
            .as_ref()
            .map(|(span, _)| span.span_id().to_string())
            .or_else(|| request.context_parent_span_id.clone());
        let ctx = self
            .prepare_tool_context(request.trace_ctx.as_ref(), tool_ctx_parent_span_id)
            .await;

        let (result, is_error, stopped, trace_status, end_name) = tokio::select! {
            exec_res = tokio::time::timeout(
                request.timeout,
                tool.execute(request.args.clone(), &ctx)
            ) => {
                match exec_res {
                    Ok(Ok(res)) => (res, false, false, TraceStatus::Ok, request.span.as_ref().map(|span| span.end_names.success)),
                    Ok(Err(err)) => (format!("Tool error: {}", err), true, false, TraceStatus::Error, request.span.as_ref().map(|span| span.end_names.error)),
                    Err(err) => (format!("Timeout executing {}: {}", request.tool_name, err), true, false, TraceStatus::TimedOut, request.span.as_ref().map(|span| span.end_names.timeout)),
                }
            }
            _ = self.cancel_token.cancelled() => {
                ("Tool execution interrupted by user.".to_string(), true, true, TraceStatus::Cancelled, request.span.as_ref().map(|span| span.end_names.cancelled))
            }
        };

        if let Some((span, span_config)) = invocation_span.take() {
            span.finish(
                end_name.unwrap_or(span_config.end_names.success),
                trace_status,
                Some(AgentContext::truncate_chars(&result, 240)),
                merge_trace_attrs(
                    span_config.end_attrs,
                    serde_json::json!({
                        "tool_name": request.tool_name,
                        "result_preview": AgentContext::truncate_chars(&result, 500),
                        "result_size_chars": result.chars().count(),
                    }),
                ),
            );
        }

        ToolInvocationOutcome {
            result,
            is_error,
            stopped,
        }
    }

    fn remaining_session_timeout_sec(&self) -> Option<u64> {
        self.session_deadline.map(|deadline| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            remaining.as_secs().max(1)
        })
    }
}

fn merge_trace_attrs(base: Value, extra: Value) -> Value {
    match (base, extra) {
        (Value::Object(mut base), Value::Object(extra)) => {
            base.extend(extra);
            Value::Object(base)
        }
        (_, extra) => extra,
    }
}
