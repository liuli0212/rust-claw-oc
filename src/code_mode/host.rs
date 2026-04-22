use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use super::protocol::{RuntimeEvent, ToolCallRequestEvent};
use crate::context::AgentContext;
use crate::tools::invocation::{
    ToolCallOrigin, ToolExecutionRequest, ToolInvocationEndNames, ToolInvocationSpanConfig,
    UnifiedToolExecutor,
};
use crate::trace::{TraceActor, TraceContext};

#[derive(Debug, Clone)]
pub(crate) struct RuntimeToolRequest {
    pub(crate) cell_id: String,
    pub(crate) seq: u64,
    pub(crate) request_id: String,
    pub(crate) tool_name: String,
    pub(crate) args_json: String,
    pub(crate) outer_tool_call_id: Option<String>,
}

#[async_trait]
pub(crate) trait CellRuntimeHost: Send + Sync {
    fn visible_tool_names(&self) -> Vec<String>;
    fn emit_event(&self, event: RuntimeEvent);
    fn cancellation_reason(&self) -> Option<String>;
    async fn call_tool(
        &self,
        request: RuntimeToolRequest,
    ) -> Result<String, crate::tools::ToolError>;
}

pub(crate) fn create_executor_host_builder(
    visible_tools: Vec<String>,
    tool_executor: Arc<tokio::sync::Mutex<UnifiedToolExecutor>>,
    trace_ctx: Option<TraceContext>,
    parent_span_id: Option<String>,
    outer_tool_call_id: Option<String>,
    provider: String,
    model: String,
) -> crate::code_mode::service::HostBuilder {
    Box::new(move |cell_id, event_tx, cancel_flag| {
        Arc::new(ExecutorCellRuntimeHost {
            cell_id,
            visible_tools,
            tool_executor,
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
    pub(crate) cell_id: String,
    pub(crate) visible_tools: Vec<String>,
    pub(crate) tool_executor: Arc<tokio::sync::Mutex<UnifiedToolExecutor>>,
    pub(crate) trace_ctx: Option<TraceContext>,
    pub(crate) parent_span_id: Option<String>,
    pub(crate) outer_tool_call_id: Option<String>,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) event_tx: tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
    pub(crate) cancel_flag: Arc<AtomicBool>,
}

impl ExecutorCellRuntimeHost {
    fn emit_tool_done(&self, request: &RuntimeToolRequest, ok: bool) {
        self.emit_event(RuntimeEvent::ToolCallDone {
            seq: request.seq,
            request_id: request.request_id.clone(),
            ok,
        });
    }

    fn span_config(
        &self,
        request: &RuntimeToolRequest,
        args: &serde_json::Value,
    ) -> ToolInvocationSpanConfig {
        let attrs = serde_json::json!({
            "tool_name": request.tool_name,
            "cell_id": request.cell_id,
            "request_id": request.request_id,
            "outer_tool_call_id": request.outer_tool_call_id,
            "provider": self.provider,
            "model": self.model,
        });
        let mut start_attrs = attrs.clone();
        start_attrs.as_object_mut().unwrap().insert(
            "args_preview".to_string(),
            serde_json::json!(AgentContext::truncate_chars(&args.to_string(), 500)),
        );

        ToolInvocationSpanConfig {
            actor: TraceActor::Tool,
            start_name: "code_mode_nested_tool_started",
            start_attrs,
            end_names: ToolInvocationEndNames {
                success: "code_mode_nested_tool_finished",
                error: "code_mode_nested_tool_failed",
                timeout: "code_mode_nested_tool_timed_out",
                cancelled: "code_mode_nested_tool_cancelled",
            },
            end_attrs: attrs,
        }
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

    fn cancellation_reason(&self) -> Option<String> {
        self.cancel_flag
            .load(Ordering::Acquire)
            .then(|| "Code mode cell execution was cancelled.".to_string())
    }

    async fn call_tool(
        &self,
        mut request: RuntimeToolRequest,
    ) -> Result<String, crate::tools::ToolError> {
        request.cell_id = self.cell_id.clone();
        if request.outer_tool_call_id.is_none() {
            request.outer_tool_call_id = self.outer_tool_call_id.clone();
        }

        self.emit_event(RuntimeEvent::ToolCallRequested(ToolCallRequestEvent {
            seq: request.seq,
            request_id: request.request_id.clone(),
            tool_name: request.tool_name.clone(),
            args_json: request.args_json.clone(),
        }));

        let args = match serde_json::from_str::<serde_json::Value>(&request.args_json) {
            Ok(args) => args,
            Err(err) => {
                self.emit_tool_done(&request, false);
                return Err(crate::tools::ToolError::InvalidArguments(format!(
                    "Invalid JSON arguments for nested tool `{}`: {err}",
                    request.tool_name
                )));
            }
        };

        if let Some(reason) = self.cancellation_reason() {
            self.emit_tool_done(&request, false);
            return Err(crate::tools::ToolError::Cancelled(reason));
        }

        let outcome = {
            let executor = self.tool_executor.lock().await;
            executor
                .execute(ToolExecutionRequest {
                    tool_name: request.tool_name.clone(),
                    args: args.clone(),
                    origin: ToolCallOrigin::CodeModeNested {
                        cell_id: Some(request.cell_id.clone()),
                        outer_tool_call_id: request.outer_tool_call_id.clone(),
                        request_id: Some(request.request_id.clone()),
                        seq: Some(request.seq),
                    },
                    timeout: Duration::from_secs(85),
                    trace_ctx: self.trace_ctx.clone(),
                    context_parent_span_id: self.parent_span_id.clone(),
                    span: Some(self.span_config(&request, &args)),
                })
                .await
        };

        let ok = !outcome.stopped && !outcome.is_error;
        self.emit_tool_done(&request, ok);
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
    pub(crate) cancel_flag: Arc<AtomicBool>,
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

    fn cancellation_reason(&self) -> Option<String> {
        self.cancel_flag
            .load(Ordering::Acquire)
            .then(|| "Code mode cell execution was cancelled.".to_string())
    }

    async fn call_tool(
        &self,
        request: RuntimeToolRequest,
    ) -> Result<String, crate::tools::ToolError> {
        self.emit_event(RuntimeEvent::ToolCallRequested(ToolCallRequestEvent {
            seq: request.seq,
            request_id: request.request_id.clone(),
            tool_name: request.tool_name.clone(),
            args_json: request.args_json.clone(),
        }));
        self.emit_event(RuntimeEvent::ToolCallDone {
            seq: request.seq,
            request_id: request.request_id,
            ok: false,
        });
        Err(crate::tools::ToolError::ExecutionFailed(
            "test event bridge host cannot execute tools".to_string(),
        ))
    }
}
