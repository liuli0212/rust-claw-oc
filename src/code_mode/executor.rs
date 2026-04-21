use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::Notify;

use crate::context::AgentContext;
use crate::core::extensions::ExecutionExtension;
use crate::core::ExecutionGuardState;
use crate::tools::invocation::{
    ToolInvocationEndNames, ToolInvocationRequest, ToolInvocationSpanConfig, ToolInvoker,
    ToolInvokerConfig,
};
use crate::tools::{Tool, ToolError};
use crate::trace::{TraceActor, TraceBus, TraceContext};

pub fn is_code_mode_nested_tool(tool_name: &str) -> bool {
    !matches!(
        tool_name,
        "exec"
            | "wait"
            | "finish_task"
            | "ask_user_question"
            | "subagent"
            | "task_plan"
            | "manage_schedule"
            | "send_telegram_message"
    )
}

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
    tool_invoker: ToolInvoker,
    remaining_steps: usize,
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

        let tool_invoker = ToolInvoker::new(ToolInvokerConfig {
            current_tools,
            visible_tools,
            extensions,
            session_id,
            reply_to,
            remaining_steps,
            session_deadline,
            trace_bus,
            cancel_token,
            is_autopilot,
            todos_path,
            execution_guard_state,
        });

        Self {
            tool_invoker,
            remaining_steps,
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
        if !self.tool_invoker.visible_tools().contains(&tool_name) {
            return Err(ToolError::ExecutionFailed(format!(
                "Tool `{tool_name}` is not available inside code mode."
            )));
        }

        if self.remaining_steps == 0 {
            return Err(ToolError::ExecutionFailed(
                "No remaining steps: nested tool call limit reached.".to_string(),
            ));
        }

        if let Some(reason) = self
            .tool_invoker
            .autopilot_denial_for_call(&tool_name, &args)
        {
            return Err(ToolError::ExecutionFailed(reason));
        }

        self.remaining_steps = self.remaining_steps.saturating_sub(1);
        self.tool_invoker.decrement_remaining_steps();

        let outcome = self
            .tool_invoker
            .invoke(ToolInvocationRequest {
                tool_name: &tool_name,
                args: args.clone(),
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
                        "remaining_steps": self.remaining_steps,
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

        if let Some(signal) =
            self.tool_invoker
                .record_action_outcome(&tool_name, &args, outcome.is_error)
        {
            return Err(ToolError::ExecutionFailed(signal.message().to_string()));
        }

        if outcome.stopped || outcome.is_error {
            return Err(ToolError::ExecutionFailed(outcome.result));
        }

        Ok(outcome.result)
    }
}
