use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::Notify;

use crate::context::AgentContext;
use crate::core::extensions::ExecutionExtension;
use crate::core::ExecutionGuardState;
use crate::tools::invocation::{
    StepBudgetHandle, ToolCallOrigin, ToolExecutionRequest, ToolInvocationEndNames,
    ToolInvocationSpanConfig, UnifiedToolExecutor, UnifiedToolExecutorConfig,
};
use crate::tools::{Tool, ToolError};
use crate::trace::{TraceActor, TraceBus, TraceContext};

pub(crate) struct CodeModeNestedToolExecutorConfig {
    pub(crate) current_tools: Vec<Arc<dyn Tool>>,
    pub(crate) visible_tools: Vec<String>,
    pub(crate) extensions: Vec<Arc<dyn ExecutionExtension>>,
    pub(crate) session_id: String,
    pub(crate) reply_to: String,
    pub(crate) remaining_steps: usize,
    pub(crate) session_deadline: Option<Instant>,
    pub(crate) iteration_trace_ctx: Option<TraceContext>,
    pub(crate) parent_span_id: Option<String>,
    pub(crate) outer_tool_call_id: Option<String>,
    pub(crate) trace_bus: Arc<TraceBus>,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) cancel_token: Arc<Notify>,
    pub(crate) is_autopilot: bool,
    pub(crate) todos_path: PathBuf,
    pub(crate) execution_guard_state: Arc<std::sync::Mutex<ExecutionGuardState>>,
}

pub(crate) struct CodeModeNestedToolExecutor {
    tool_executor: UnifiedToolExecutor,
    iteration_trace_ctx: Option<TraceContext>,
    parent_span_id: Option<String>,
    outer_tool_call_id: Option<String>,
    provider: String,
    model: String,
}

impl CodeModeNestedToolExecutor {
    pub(crate) fn new(config: CodeModeNestedToolExecutorConfig) -> Self {
        let CodeModeNestedToolExecutorConfig {
            current_tools,
            visible_tools,
            extensions,
            session_id,
            reply_to,
            remaining_steps,
            session_deadline,
            iteration_trace_ctx,
            parent_span_id,
            outer_tool_call_id,
            trace_bus,
            provider,
            model,
            cancel_token,
            is_autopilot,
            todos_path,
            execution_guard_state,
        } = config;

        let tool_executor = UnifiedToolExecutor::new(UnifiedToolExecutorConfig {
            current_tools,
            visible_tools,
            extensions,
            session_id,
            reply_to,
            step_budget: StepBudgetHandle::new(remaining_steps),
            session_deadline,
            trace_bus,
            cancel_token,
            is_autopilot,
            todos_path,
            execution_guard_state,
        });

        Self {
            tool_executor,
            iteration_trace_ctx,
            parent_span_id,
            outer_tool_call_id,
            provider,
            model,
        }
    }

    pub(crate) async fn execute_json(
        &mut self,
        tool_name: String,
        args_json: String,
    ) -> Result<String, ToolError> {
        let args = serde_json::from_str::<Value>(&args_json).map_err(|err| {
            ToolError::InvalidArguments(format!(
                "Invalid JSON arguments for nested tool `{tool_name}`: {err}"
            ))
        })?;
        self.execute(tool_name, args).await
    }

    async fn execute(&mut self, tool_name: String, args: Value) -> Result<String, ToolError> {
        let outcome = self
            .tool_executor
            .execute(ToolExecutionRequest {
                tool_name: tool_name.clone(),
                args: args.clone(),
                origin: ToolCallOrigin::CodeModeNested {
                    cell_id: None,
                    outer_tool_call_id: self.outer_tool_call_id.clone(),
                    request_id: None,
                    seq: None,
                },
                timeout: Duration::from_secs(85),
                trace_ctx: self.iteration_trace_ctx.clone(),
                context_parent_span_id: self.parent_span_id.clone(),
                span: Some(ToolInvocationSpanConfig {
                    actor: TraceActor::Tool,
                    start_name: "code_mode_nested_tool_started",
                    start_attrs: serde_json::json!({
                        "tool_name": tool_name.clone(),
                        "outer_tool_call_id": self.outer_tool_call_id.clone(),
                        "provider": self.provider.clone(),
                        "model": self.model.clone(),
                        "args_preview": AgentContext::truncate_chars(&args.to_string(), 500),
                        "remaining_steps": self.tool_executor.remaining_steps().saturating_sub(1),
                    }),
                    end_names: ToolInvocationEndNames {
                        success: "code_mode_nested_tool_finished",
                        error: "code_mode_nested_tool_failed",
                        timeout: "code_mode_nested_tool_timed_out",
                        cancelled: "code_mode_nested_tool_cancelled",
                    },
                    end_attrs: serde_json::json!({
                        "tool_name": tool_name.clone(),
                        "outer_tool_call_id": self.outer_tool_call_id.clone(),
                        "provider": self.provider.clone(),
                        "model": self.model.clone(),
                    }),
                }),
            })
            .await;

        if outcome.stopped || outcome.is_error {
            return Err(ToolError::ExecutionFailed(outcome.result));
        }

        Ok(outcome.result)
    }
}
