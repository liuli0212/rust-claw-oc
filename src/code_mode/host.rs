use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use super::protocol::{RuntimeEvent, ToolCallRequest};
use crate::context::AgentContext;
use crate::tools::invocation::{ToolCallOrigin, ToolExecutionRequest, UnifiedToolExecutor};
use crate::trace::{TraceActor, TraceBus, TraceContext, TraceStatus};

#[async_trait]
pub(crate) trait CellRuntimeHost: Send + Sync {
    fn visible_tool_names(&self) -> Vec<String>;
    fn emit_event(&self, event: RuntimeEvent);
    async fn call_tool(&self, request: ToolCallRequest) -> Result<String, crate::tools::ToolError>;
}

pub(crate) fn create_executor_host_builder(
    visible_tools: Vec<String>,
    tool_executor: Arc<tokio::sync::Mutex<UnifiedToolExecutor>>,
    trace_bus: Arc<TraceBus>,
    trace_ctx: Option<TraceContext>,
    parent_span_id: Option<String>,
    outer_tool_call_id: Option<String>,
    provider_model: (String, String),
) -> crate::code_mode::service::HostBuilder {
    let (provider, model) = provider_model;
    Box::new(move |cell_id, event_tx, cancel_flag| {
        Arc::new(ExecutorCellRuntimeHost {
            cell_id,
            visible_tools,
            tool_executor,
            trace_bus,
            trace_ctx,
            parent_span_id,
            outer_tool_call_id,
            provider,
            model,
            event_tx,
            cancel_flag,
        })
    })
}

pub(crate) struct ExecutorCellRuntimeHost {
    cell_id: String,
    visible_tools: Vec<String>,
    tool_executor: Arc<tokio::sync::Mutex<UnifiedToolExecutor>>,
    trace_bus: Arc<TraceBus>,
    trace_ctx: Option<TraceContext>,
    parent_span_id: Option<String>,
    outer_tool_call_id: Option<String>,
    provider: String,
    model: String,
    event_tx: tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
    cancel_flag: Arc<AtomicBool>,
}

impl ExecutorCellRuntimeHost {
    fn emit_tool_done(&self, request: &ToolCallRequest) {
        self.emit_event(RuntimeEvent::ToolCallDone {
            seq: request.seq,
            request_id: request.request_id.clone(),
        });
    }

    fn tool_trace_attrs(&self, request: &ToolCallRequest) -> serde_json::Value {
        serde_json::json!({
            "tool_name": request.tool_name,
            "cell_id": self.cell_id,
            "request_id": request.request_id,
            "outer_tool_call_id": self.outer_tool_call_id,
            "provider": self.provider,
            "model": self.model,
        })
    }
}

#[async_trait]
impl CellRuntimeHost for ExecutorCellRuntimeHost {
    fn visible_tool_names(&self) -> Vec<String> {
        self.visible_tools.clone()
    }

    fn emit_event(&self, event: RuntimeEvent) {
        let _ = self.event_tx.send(event);
    }

    async fn call_tool(&self, request: ToolCallRequest) -> Result<String, crate::tools::ToolError> {
        self.emit_event(RuntimeEvent::ToolCallRequested(request.clone()));

        let args = match serde_json::from_str::<serde_json::Value>(&request.args_json) {
            Ok(args) => args,
            Err(err) => {
                self.emit_tool_done(&request);
                return Err(crate::tools::ToolError::InvalidArguments(format!(
                    "Invalid JSON arguments for nested tool `{}`: {err}",
                    request.tool_name
                )));
            }
        };

        if self.cancel_flag.load(Ordering::Acquire) {
            self.emit_tool_done(&request);
            return Err(crate::tools::ToolError::Cancelled(
                "Code mode cell execution was cancelled.".to_string(),
            ));
        }

        let trace_attrs = self.tool_trace_attrs(&request);
        let mut start_attrs = trace_attrs.clone();
        if let Some(attrs) = start_attrs.as_object_mut() {
            attrs.insert(
                "args_preview".to_string(),
                serde_json::json!(AgentContext::truncate_chars(&args.to_string(), 500)),
            );
        }
        let tool_span = self.trace_ctx.as_ref().map(|trace_ctx| {
            let span_ctx = trace_ctx.with_parent_span_id(self.parent_span_id.clone());
            self.trace_bus.start_span(
                &span_ctx,
                TraceActor::Tool,
                "code_mode_nested_tool_started",
                start_attrs,
            )
        });
        let context_parent_span_id = tool_span
            .as_ref()
            .map(|span| span.span_id().to_string())
            .or_else(|| self.parent_span_id.clone());

        let outcome = {
            let executor = self.tool_executor.lock().await;
            executor
                .execute(ToolExecutionRequest {
                    tool_name: request.tool_name.clone(),
                    args: args.clone(),
                    origin: ToolCallOrigin::CodeModeNested,
                    timeout: Duration::from_secs(85),
                    trace_ctx: self.trace_ctx.clone(),
                    context_parent_span_id,
                })
                .await
        };

        if let Some(span) = tool_span {
            let (event_name, status) = if outcome.stopped {
                ("code_mode_nested_tool_cancelled", TraceStatus::Cancelled)
            } else if outcome.timed_out {
                ("code_mode_nested_tool_timed_out", TraceStatus::TimedOut)
            } else if outcome.is_error {
                ("code_mode_nested_tool_failed", TraceStatus::Error)
            } else {
                ("code_mode_nested_tool_finished", TraceStatus::Ok)
            };
            let mut end_attrs = trace_attrs;
            if let Some(attrs) = end_attrs.as_object_mut() {
                attrs.insert("origin".to_string(), serde_json::json!("code_mode_nested"));
                attrs.insert("seq".to_string(), serde_json::json!(request.seq));
                attrs.insert(
                    "result_preview".to_string(),
                    serde_json::json!(AgentContext::truncate_chars(&outcome.result, 500)),
                );
                attrs.insert(
                    "result_size_chars".to_string(),
                    serde_json::json!(outcome.result.chars().count()),
                );
            }
            span.finish(
                event_name,
                status,
                Some(AgentContext::truncate_chars(&outcome.result, 240)),
                end_attrs,
            );
        }

        let ok = !outcome.stopped && !outcome.is_error;
        self.emit_tool_done(&request);
        if ok {
            Ok(crate::code_mode::runtime::value::normalize_tool_result_for_js(&outcome.result))
        } else {
            Err(crate::tools::ToolError::ExecutionFailed(outcome.result))
        }
    }
}

#[cfg(test)]
pub(crate) struct EventBridgeHost {
    pub(crate) visible_tools: Vec<String>,
    pub(crate) event_tx: tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
}

#[cfg(test)]
#[async_trait]
impl CellRuntimeHost for EventBridgeHost {
    fn visible_tool_names(&self) -> Vec<String> {
        self.visible_tools.clone()
    }

    fn emit_event(&self, event: RuntimeEvent) {
        let _ = self.event_tx.send(event);
    }

    async fn call_tool(
        &self,
        _request: ToolCallRequest,
    ) -> Result<String, crate::tools::ToolError> {
        Err(crate::tools::ToolError::ExecutionFailed(
            "test event bridge host cannot execute tools".to_string(),
        ))
    }
}
