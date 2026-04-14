use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::Notify;

use crate::context::AgentContext;
use crate::core::extensions::ExecutionExtension;
use crate::tools::protocol::ToolTraceContext;
use crate::tools::{Tool, ToolContext, ToolError};
use crate::trace::{TraceActor, TraceBus, TraceContext, TraceStatus};

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

pub struct CodeModeNestedToolExecutorConfig<'a> {
    pub current_tools: Vec<Arc<dyn Tool>>,
    pub extensions: &'a [Box<dyn ExecutionExtension>],
    pub session_id: String,
    pub reply_to: String,
    pub remaining_steps: usize,
    pub session_deadline: Option<Instant>,
    pub iteration_trace_ctx: Option<TraceContext>,
    pub parent_span_id: Option<String>,
    pub outer_tool_call_id: Option<String>,
    pub trace_bus: Arc<TraceBus>,
    pub provider: String,
    pub model: String,
    pub cancel_token: Arc<Notify>,
    pub is_autopilot: bool,
    pub todos_path: PathBuf,
    pub action_history: &'a mut VecDeque<String>,
    pub reflection_strike: &'a mut u8,
}

pub struct CodeModeNestedToolExecutor<'a> {
    current_tools: Vec<Arc<dyn Tool>>,
    extensions: &'a [Box<dyn ExecutionExtension>],
    session_id: String,
    reply_to: String,
    visible_tools: Vec<String>,
    remaining_steps: usize,
    session_deadline: Option<Instant>,
    iteration_trace_ctx: Option<TraceContext>,
    parent_span_id: Option<String>,
    outer_tool_call_id: Option<String>,
    trace_bus: Arc<TraceBus>,
    provider: String,
    model: String,
    cancel_token: Arc<Notify>,
    is_autopilot: bool,
    todos_path: PathBuf,
    action_history: &'a mut VecDeque<String>,
    reflection_strike: &'a mut u8,
}

impl<'a> CodeModeNestedToolExecutor<'a> {
    pub fn new(config: CodeModeNestedToolExecutorConfig<'a>) -> Self {
        let visible_tools = config
            .current_tools
            .iter()
            .map(|tool| tool.name())
            .collect();
        Self {
            current_tools: config.current_tools,
            extensions: config.extensions,
            session_id: config.session_id,
            reply_to: config.reply_to,
            visible_tools,
            remaining_steps: config.remaining_steps,
            session_deadline: config.session_deadline,
            iteration_trace_ctx: config.iteration_trace_ctx,
            parent_span_id: config.parent_span_id,
            outer_tool_call_id: config.outer_tool_call_id,
            trace_bus: config.trace_bus,
            provider: config.provider,
            model: config.model,
            cancel_token: config.cancel_token,
            is_autopilot: config.is_autopilot,
            todos_path: config.todos_path,
            action_history: config.action_history,
            reflection_strike: config.reflection_strike,
        }
    }

    pub async fn execute_json(
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
        if !is_code_mode_nested_tool(&tool_name) {
            return Err(ToolError::ExecutionFailed(format!(
                "Tool `{tool_name}` is not available inside code mode."
            )));
        }

        if let Some(reason) = self.autopilot_denial_for_call(&tool_name, &args) {
            return Err(ToolError::ExecutionFailed(reason));
        }

        let nested_trace_ctx = self
            .iteration_trace_ctx
            .as_ref()
            .map(|ctx| ctx.with_parent_span_id(self.parent_span_id.clone()));
        let mut nested_span = nested_trace_ctx.as_ref().map(|ctx| {
            self.trace_bus.start_span(
                ctx,
                TraceActor::Tool,
                "code_mode_nested_tool_started",
                serde_json::json!({
                    "tool_name": tool_name.clone(),
                    "outer_tool_call_id": self.outer_tool_call_id.clone(),
                    "provider": self.provider.clone(),
                    "model": self.model.clone(),
                    "args_preview": AgentContext::truncate_chars(&args.to_string(), 500),
                    "remaining_steps": self.remaining_steps,
                }),
            )
        });

        let nested_ctx = self
            .prepare_tool_context(
                nested_trace_ctx.as_ref(),
                nested_span.as_ref().map(|span| span.span_id().to_string()),
            )
            .await;

        let (result, is_error, stopped, trace_status, end_name) = {
            let tool = self
                .current_tools
                .iter()
                .find(|tool| tool.name() == tool_name)
                .ok_or_else(|| {
                    ToolError::ExecutionFailed(format!("Tool not found: {tool_name}"))
                })?;

            tokio::select! {
                exec_res = tokio::time::timeout(
                    Duration::from_secs(120),
                    tool.execute(args.clone(), &nested_ctx)
                ) => {
                    match exec_res {
                        Ok(Ok(res)) => (res, false, false, TraceStatus::Ok, "code_mode_nested_tool_finished"),
                        Ok(Err(err)) => (format!("Tool error: {}", err), true, false, TraceStatus::Error, "code_mode_nested_tool_failed"),
                        Err(err) => (format!("Timeout executing {}: {}", tool_name, err), true, false, TraceStatus::TimedOut, "code_mode_nested_tool_timed_out"),
                    }
                }
                _ = self.cancel_token.notified() => {
                    ("Tool execution interrupted by user.".to_string(), true, true, TraceStatus::Cancelled, "code_mode_nested_tool_cancelled")
                }
            }
        };

        if let Some(span) = nested_span.take() {
            span.finish(
                end_name,
                trace_status,
                Some(AgentContext::truncate_chars(&result, 240)),
                serde_json::json!({
                    "tool_name": tool_name.clone(),
                    "outer_tool_call_id": self.outer_tool_call_id.clone(),
                    "provider": self.provider.clone(),
                    "model": self.model.clone(),
                    "result_preview": AgentContext::truncate_chars(&result, 500),
                    "result_size_chars": result.chars().count(),
                }),
            );
        }

        if let Some(message) = self.record_autopilot_action_outcome(&tool_name, &args, is_error) {
            return Err(ToolError::ExecutionFailed(message));
        }

        if stopped || is_error {
            return Err(ToolError::ExecutionFailed(result));
        }

        Ok(result)
    }

    async fn prepare_tool_context(
        &self,
        trace_ctx: Option<&TraceContext>,
        parent_span_id: Option<String>,
    ) -> ToolContext {
        let mut ctx = ToolContext::new(self.session_id.clone(), self.reply_to.clone());
        ctx.visible_tools = self.visible_tools.clone();
        ctx.skill_budget.remaining_steps = Some(self.remaining_steps);
        ctx.skill_budget.remaining_timeout_sec = self.remaining_session_timeout_sec();
        if let Some(trace_ctx) = trace_ctx {
            ctx.trace = Some(ToolTraceContext {
                trace_id: trace_ctx.trace_id.clone(),
                run_id: trace_ctx.run_id.clone(),
                root_session_id: trace_ctx.root_session_id.clone(),
                task_id: trace_ctx.task_id.clone(),
                turn_id: trace_ctx.turn_id.clone(),
                iteration: trace_ctx.iteration,
                parent_span_id,
            });
        }
        for ext in self.extensions {
            ctx = ext.enrich_tool_context(ctx).await;
        }
        ctx
    }

    fn remaining_session_timeout_sec(&self) -> Option<u64> {
        self.session_deadline.map(|deadline| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            remaining.as_secs().max(1)
        })
    }

    fn autopilot_denial_for_call(&self, call_name: &str, call_args: &Value) -> Option<String> {
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

        let is_creating_todos = (call_name == "write_file" || call_name == "execute_bash")
            && call_args.to_string().contains("TODOS.md");
        if is_creating_todos {
            return None;
        }

        Some(
            "[System Error] Action Denied. Autopilot 模式下必须先创建并规划 TODOS.md。".to_string(),
        )
    }

    fn record_autopilot_action_outcome(
        &mut self,
        call_name: &str,
        call_args: &Value,
        is_error: bool,
    ) -> Option<String> {
        if !self.is_autopilot {
            return None;
        }

        let action_key = format!("{}:{}:{}", call_name, call_args, is_error);
        self.action_history.push_back(action_key.clone());
        if self.action_history.len() > 3 {
            self.action_history.pop_front();
        }

        if self.action_history.len() == 3
            && self.action_history.iter().all(|key| key == &action_key)
        {
            *self.reflection_strike += 1;
            self.action_history.clear();

            if *self.reflection_strike >= 2 {
                return Some("[System Error] 检测到深度死循环，反思无效。".to_string());
            }

            return Some(
                "[System Warning] 检测到你正在重复执行相同的错误动作。请立即停止当前尝试，反思失败原因，并提出全新的解决路径。".to_string(),
            );
        }

        None
    }
}
