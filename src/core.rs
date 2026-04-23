use crate::context::{AgentContext, ContextDiff, FunctionResponse, Message, Part};
use crate::llm_client::{LlmClient, StreamEvent};
use crate::tools::Tool;
use crate::trace::{shared_bus, TraceActor, TraceContext, TraceSeed, TraceSpanHandle, TraceStatus};
use async_trait::async_trait;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

pub mod extensions;
mod step_helpers;

pub struct ScopeGuard<F: FnOnce()> {
    closure: Option<F>,
}

impl<F: FnOnce()> ScopeGuard<F> {
    pub fn new(closure: F) -> Self {
        Self {
            closure: Some(closure),
        }
    }
}

impl<F: FnOnce()> Drop for ScopeGuard<F> {
    fn drop(&mut self) {
        if let Some(closure) = self.closure.take() {
            closure();
        }
    }
}

#[async_trait]
pub trait AgentOutput: Send + Sync {
    async fn on_waiting(&self, _message: &str) {}
    fn clear_waiting(&self) {}
    async fn on_text(&self, text: &str);
    async fn on_thinking(&self, text: &str) {
        // Default: treat thinking as regular text (backward compat)
        self.on_text(text).await;
    }
    async fn on_tool_start(&self, name: &str, args: &str);
    async fn on_tool_end(&self, result: &str);
    async fn on_error(&self, error: &str);
    async fn flush(&self) {
        // Default: no-op (CLI doesn't need buffering)
    }
    async fn on_file(&self, path: &str) {
        // Default: just notify that a file was created
        self.on_text(&format!("[File] Created: {}\n", path)).await;
    }
    async fn on_plan_update(&self, _state: &crate::task_state::TaskStateSnapshot) {
        // Default: no-op
    }
    async fn on_status_update(
        &self,
        _tokens: usize,
        _max_tokens: usize,
        _energy: usize,
        _provider: &str,
        _model: &str,
    ) {
        // Default: no-op
    }
    async fn on_task_finish(&self, _summary: &str) {
        // Default: no-op
    }
    async fn on_llm_request(&self, _prompt_summary: &str) {}
    async fn on_llm_response(&self, _response_summary: &str) {}
}

pub struct SilentOutputWrapper {
    pub inner: Arc<dyn AgentOutput>,
}

#[async_trait]
impl AgentOutput for SilentOutputWrapper {
    async fn on_waiting(&self, message: &str) {
        self.inner.on_waiting(message).await;
    }
    fn clear_waiting(&self) {
        self.inner.clear_waiting();
    }
    async fn on_text(&self, text: &str) {
        self.inner.on_text(text).await;
    }
    async fn on_thinking(&self, _text: &str) {}
    async fn on_tool_start(&self, _name: &str, _args: &str) {}
    async fn on_tool_end(&self, _result: &str) {}
    async fn on_error(&self, error: &str) {
        self.inner.on_error(error).await;
    }
    async fn flush(&self) {
        self.inner.flush().await;
    }
    async fn on_file(&self, path: &str) {
        self.inner.on_file(path).await;
    }
    async fn on_plan_update(&self, state: &crate::task_state::TaskStateSnapshot) {
        self.inner.on_plan_update(state).await;
    }
    async fn on_status_update(
        &self,
        tokens: usize,
        max_tokens: usize,
        energy: usize,
        provider: &str,
        model: &str,
    ) {
        self.inner
            .on_status_update(tokens, max_tokens, energy, provider, model)
            .await;
    }
    async fn on_task_finish(&self, summary: &str) {
        self.inner.on_task_finish(summary).await;
    }
    async fn on_llm_request(&self, prompt_summary: &str) {
        self.inner.on_llm_request(prompt_summary).await;
    }
    async fn on_llm_response(&self, response_summary: &str) {
        self.inner.on_llm_response(response_summary).await;
    }
}

pub trait OutputRouter: Send + Sync {
    fn try_route(&self, reply_to: &str) -> Option<Arc<dyn AgentOutput>>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum RunExit {
    Finished(String),
    StoppedByUser,
    YieldedToUser,
    RecoverableFailed(String),
    CriticallyFailed(String),
    AutopilotStalled(String),
    EnergyDepleted(String),
}

impl RunExit {
    pub fn label(&self) -> &'static str {
        match self {
            RunExit::Finished(_) => "finished",
            RunExit::StoppedByUser => "stopped_by_user",
            RunExit::YieldedToUser => "yielded_to_user",
            RunExit::RecoverableFailed(_) => "recoverable_failed",
            RunExit::CriticallyFailed(_) => "critically_failed",
            RunExit::AutopilotStalled(_) => "autopilot_stalled",
            RunExit::EnergyDepleted(_) => "energy_depleted",
        }
    }
}

struct TaskState {
    iterations: usize,
    energy_points: usize,
}

type ToolCallRecord = (crate::context::FunctionCall, Option<String>);

enum StreamCollectionOutcome {
    Completed {
        full_text: String,
        tool_calls: Vec<ToolCallRecord>,
    },
    Exit(RunExit),
}

pub(crate) struct ToolDispatchOutcome {
    pub(crate) result: String,
    pub(crate) is_error: bool,
    pub(crate) stopped: bool,
    pub(crate) guard_signal: Option<ExecutionGuardSignal>,
}

struct ActiveTrace {
    base_ctx: TraceContext,
    run_span: Option<TraceSpanHandle>,
    turn_span: Option<TraceSpanHandle>,
}

#[derive(Debug, Default)]
pub(crate) struct ExecutionGuardState {
    action_history: VecDeque<String>,
    reflection_strike: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecutionGuardSignal {
    ReflectionWarning,
    AutopilotMeltdown,
}

impl ExecutionGuardSignal {
    pub(crate) fn signal(self) -> &'static str {
        match self {
            Self::ReflectionWarning => "reflection_warning",
            Self::AutopilotMeltdown => "autopilot_meltdown",
        }
    }

    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::ReflectionWarning => {
                "[System Warning] 检测到你正在重复执行相同的错误动作。请立即停止当前尝试，反思失败原因，并提出全新的解决路径。"
            }
            Self::AutopilotMeltdown => "[System Error] 检测到深度死循环，反思无效。",
        }
    }
}

impl ExecutionGuardState {
    pub(crate) fn record_action_outcome(
        &mut self,
        call_name: &str,
        call_args: &serde_json::Value,
        is_error: bool,
    ) -> Option<ExecutionGuardSignal> {
        let action_key = format!("{}:{}:{}", call_name, call_args, is_error);
        self.action_history.push_back(action_key.clone());
        if self.action_history.len() > 3 {
            self.action_history.pop_front();
        }

        if self.action_history.len() == 3 && self.action_history.iter().all(|k| k == &action_key) {
            self.reflection_strike += 1;
            self.action_history.clear();

            if self.reflection_strike >= 2 {
                return Some(ExecutionGuardSignal::AutopilotMeltdown);
            }

            return Some(ExecutionGuardSignal::ReflectionWarning);
        }

        None
    }
}

pub struct AgentLoop {
    session_id: String,
    pub reply_to: String,
    llm: Arc<dyn LlmClient>,
    tools: Vec<Arc<dyn Tool>>,
    pub context: AgentContext,
    output: Arc<dyn AgentOutput>,
    telemetry: Arc<crate::telemetry::TelemetryExporter>,
    task_state_store: Arc<crate::task_state::TaskStateStore>,
    pub cancel_token: Arc<Notify>,
    pub cancelled: Arc<std::sync::atomic::AtomicBool>,
    pub is_autopilot: bool,
    pub is_subagent: bool,
    execution_guard_state: Arc<std::sync::Mutex<ExecutionGuardState>>,
    autopilot_todos_completed_count: usize,
    autopilot_work_dir: Option<PathBuf>,
    extensions: Vec<Arc<dyn extensions::ExecutionExtension>>,
    initial_energy_budget: usize,
    session_deadline: Option<Instant>,
    trace_bus: Arc<crate::trace::TraceBus>,
    trace_seed: Option<TraceSeed>,
    active_trace: Option<ActiveTrace>,
    code_mode_service: crate::code_mode::service::CodeModeService,
}

impl AgentLoop {
    const MAX_LLM_RECOVERY_ATTEMPTS: usize = 3;
    const MAX_CONSECUTIVE_EMPTY_RESPONSES: usize = 3;
    const INITIAL_ENERGY: usize = 25; // 每轮最大生存时间（步数），AutoPilot中由物理审计自动续期

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: String,
        llm: Arc<dyn LlmClient>,
        reply_to: String,
        tools: Vec<Arc<dyn Tool>>,
        context: AgentContext,
        output: Arc<dyn AgentOutput>,
        telemetry: Arc<crate::telemetry::TelemetryExporter>,
        task_state_store: Arc<crate::task_state::TaskStateStore>,
    ) -> Self {
        Self {
            session_id,
            llm,
            reply_to,
            tools,
            context,
            output,
            telemetry,
            task_state_store,
            cancel_token: Arc::new(Notify::new()),
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            is_autopilot: false,
            is_subagent: false,
            execution_guard_state: Arc::new(std::sync::Mutex::new(ExecutionGuardState::default())),
            autopilot_todos_completed_count: 0,
            autopilot_work_dir: None,
            extensions: Vec::new(),
            initial_energy_budget: Self::INITIAL_ENERGY,
            session_deadline: None,
            trace_bus: shared_bus(),
            trace_seed: None,
            active_trace: None,
            code_mode_service: crate::code_mode::service::CodeModeService::default(),
        }
    }

    /// Register an execution extension (e.g. SkillRuntime).
    pub fn add_extension(&mut self, ext: Arc<dyn extensions::ExecutionExtension>) {
        self.extensions.push(ext);
    }

    pub fn set_initial_energy_budget(&mut self, energy_budget: usize) {
        self.initial_energy_budget = energy_budget.max(1);
    }

    pub fn set_session_timeout(&mut self, timeout: Duration) {
        self.session_deadline = Some(Instant::now() + timeout);
    }

    pub fn set_trace_seed(&mut self, trace_seed: TraceSeed) {
        self.trace_seed = Some(trace_seed);
    }

    pub fn remaining_session_timeout_sec(&self) -> Option<u64> {
        self.session_deadline.map(|deadline| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            remaining.as_secs().max(1)
        })
    }

    pub fn request_cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.cancel_token.notify_waiters();
    }

    pub async fn abort_active_code_mode(&self, reason: &str) -> bool {
        self.code_mode_service
            .abort_active_cell(&self.session_id, reason)
            .await
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn update_llm(&mut self, new_llm: Arc<dyn LlmClient>) {
        self.llm = new_llm;
    }
    pub fn update_output(&mut self, output: Arc<dyn AgentOutput>) {
        self.output = output;
    }

    pub(crate) fn trace_actor(&self) -> TraceActor {
        if self.is_subagent {
            TraceActor::Subagent
        } else {
            TraceActor::MainAgent
        }
    }

    pub(crate) fn trace_context_with_parent(
        &self,
        parent_span_id: Option<String>,
        iteration: Option<u32>,
    ) -> Option<TraceContext> {
        self.active_trace.as_ref().map(|active| {
            active
                .base_ctx
                .with_parent_span_id(parent_span_id)
                .with_iteration(iteration)
        })
    }

    pub(crate) fn turn_span_id(&self) -> Option<String> {
        self.active_trace.as_ref().and_then(|active| {
            active
                .turn_span
                .as_ref()
                .map(|span| span.span_id().to_string())
        })
    }

    fn begin_trace_run(&mut self, goal: &str, task_id: Option<String>) {
        let turn_id = self
            .context
            .current_turn
            .as_ref()
            .map(|turn| turn.turn_id.clone());
        let inherited_seed = if self.is_subagent {
            self.trace_seed.clone()
        } else {
            None
        };

        let mut base_ctx = if let Some(seed) = inherited_seed.clone() {
            crate::trace::trace_ctx_from_seed(&seed, &self.session_id)
        } else {
            let run_id = format!("run_{}", uuid::Uuid::new_v4().simple());
            TraceContext {
                trace_id: run_id.clone(),
                run_id,
                session_id: self.session_id.clone(),
                root_session_id: self.session_id.clone(),
                task_id: task_id.clone(),
                turn_id: None,
                iteration: None,
                parent_span_id: None,
            }
        };
        base_ctx.task_id = task_id;
        base_ctx.turn_id = turn_id.clone();

        let run_span = if inherited_seed.is_none() {
            let run_ctx = base_ctx.with_parent_span_id(None);
            Some(self.trace_bus.start_span(
                &run_ctx,
                self.trace_actor(),
                "run_started",
                serde_json::json!({
                    "goal": goal,
                    "provider": self.llm.provider_name(),
                    "model": self.llm.model_name(),
                    "is_subagent": self.is_subagent,
                }),
            ))
        } else {
            None
        };

        let turn_parent_span_id = run_span
            .as_ref()
            .map(|span| span.span_id().to_string())
            .or_else(|| base_ctx.parent_span_id.clone());
        let turn_ctx = base_ctx.with_parent_span_id(turn_parent_span_id);
        let turn_span = Some(self.trace_bus.start_span(
            &turn_ctx,
            self.trace_actor(),
            "turn_started",
            serde_json::json!({
                "goal": goal,
                "turn_id": turn_id,
                "is_subagent": self.is_subagent,
            }),
        ));

        self.active_trace = Some(ActiveTrace {
            base_ctx,
            run_span,
            turn_span,
        });
    }

    pub(crate) fn finish_active_trace(
        &mut self,
        end_name: &str,
        status: TraceStatus,
        summary: Option<String>,
    ) {
        let Some(active) = self.active_trace.take() else {
            return;
        };

        if let Some(turn_span) = active.turn_span {
            turn_span.finish(
                "turn_finished",
                status.clone(),
                summary.clone(),
                serde_json::json!({}),
            );
        }

        if let Some(run_span) = active.run_span {
            run_span.finish(
                end_name,
                status,
                summary,
                serde_json::json!({
                    "provider": self.llm.provider_name(),
                    "model": self.llm.model_name(),
                }),
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_trace_event(
        &self,
        actor: TraceActor,
        name: &str,
        status: TraceStatus,
        summary: Option<String>,
        attrs: serde_json::Value,
        parent_span_id: Option<String>,
        iteration: Option<u32>,
    ) {
        if let Some(ctx) = self.trace_context_with_parent(parent_span_id, iteration) {
            self.trace_bus
                .record_event(&ctx, actor, name, status, summary, attrs);
        }
    }

    /// Enable autopilot mode. MUST be called BEFORE the first `step()` call,
    /// as it captures the current working directory and initial TODOS.md baseline.
    pub fn enable_autopilot(&mut self) {
        self.is_autopilot = true;
        self.autopilot_work_dir = std::env::current_dir().ok();
        self.autopilot_todos_completed_count = self.count_completed_todos();
    }

    fn count_completed_todos(&self) -> usize {
        self.count_todos_status().0
    }

    /// Returns (completed_count, uncompleted_count) from TODOS.md.
    /// Uses the locked autopilot_work_dir if set, otherwise falls back to CWD.
    pub(crate) fn count_todos_status(&self) -> (usize, usize) {
        use std::sync::LazyLock;
        static RE_COMPLETED: LazyLock<regex::Regex> =
            LazyLock::new(|| regex::Regex::new(r"(?i)[-*]\s*\[x\]").unwrap());
        static RE_UNCOMPLETED: LazyLock<regex::Regex> =
            LazyLock::new(|| regex::Regex::new(r"(?i)[-*]\s*\[\s\]").unwrap());

        let todos_path = self.todos_path();
        let content = std::fs::read_to_string(todos_path).unwrap_or_default();
        (
            RE_COMPLETED.find_iter(&content).count(),
            RE_UNCOMPLETED.find_iter(&content).count(),
        )
    }

    /// Returns the absolute path to TODOS.md, using the locked work directory if available.
    pub(crate) fn todos_path(&self) -> PathBuf {
        if let Some(dir) = &self.autopilot_work_dir {
            dir.join("TODOS.md")
        } else {
            PathBuf::from("TODOS.md")
        }
    }

    /// Check if TODOS.md has any uncompleted items. Uses the same regex as count_todos_status.
    pub(crate) fn has_uncompleted_todos(&self) -> bool {
        self.count_todos_status().1 > 0
    }

    pub async fn flush_output(&self) {
        self.output.flush().await;
    }

    #[allow(dead_code)]
    pub fn get_session_details(&self) -> serde_json::Value {
        let (tokens, max_tokens, turns, system_tokens, _) = self.context.get_context_status();
        let state = self
            .task_state_store
            .load()
            .unwrap_or_else(|_| crate::task_state::TaskStateSnapshot::empty());
        serde_json::json!({
            "session_id": self.session_id,
            "provider": self.llm.provider_name(),
            "model": self.llm.model_name(),
            "context": {
                "tokens": tokens,
                "max_tokens": max_tokens,
                "turns": turns,
                "system_tokens": system_tokens,
            },
            "task_id": state.task_id,
            "task_status": state.status,
            "cancelled": self.is_cancelled(),
            "tools": self.get_tools_metadata(),
        })
    }

    pub fn get_tools_metadata(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.tools
                .iter()
                .map(|t| {
                    let definition = t.definition();
                    serde_json::json!({
                        "name": definition.name,
                        "description": definition.description,
                        "parameters": definition.input_schema,
                    })
                })
                .collect(),
        )
    }

    pub fn get_status(&self) -> (String, String, usize, usize) {
        let (total_tokens, max_tokens, _, _, _) = self.context.get_context_status();
        (
            self.llm.provider_name().to_string(),
            self.llm.model_name().to_string(),
            total_tokens,
            max_tokens,
        )
    }

    // Proxy methods for context inspection required by main.rs
    pub fn get_context_details(&self) -> String {
        self.context.get_context_details()
    }

    #[allow(dead_code)]
    pub fn get_detailed_stats(&self) -> crate::context::DetailedContextStats {
        self.context.get_detailed_stats(None)
    }

    pub fn diff_snapshot(&self) -> Option<ContextDiff> {
        self.context
            .last_snapshot
            .as_ref()
            .map(|old| self.context.diff_snapshot(old))
    }

    pub fn format_diff(&self, diff: &ContextDiff) -> String {
        self.context.format_diff(diff)
    }

    pub fn inspect_context(&self, section: &str, arg: Option<&str>) -> String {
        if section == "plan" {
            if let Ok(state) = self.task_state_store.load() {
                if state.plan_steps.is_empty() {
                    return "No active plan.".to_string();
                }
                return state.summary();
            } else {
                return "No active plan.".to_string();
            }
        }
        self.context.inspect_context(section, arg)
    }

    pub fn build_llm_payload(
        &self,
    ) -> (Vec<Message>, Option<Message>, crate::context::PromptReport) {
        let state = self
            .task_state_store
            .load()
            .unwrap_or_else(|_| crate::task_state::TaskStateSnapshot::empty());
        let max_tokens = self.context.max_history_tokens;
        let assembler = crate::context_assembler::ContextAssembler::new(max_tokens);
        self.context.build_llm_payload(&state, &assembler)
    }

    pub async fn maybe_compact_history(
        &mut self,
        force: bool,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (current_usage, max_tokens, _, _, _) = self.context.get_context_status();
        let threshold = (max_tokens as f64 * 0.85) as usize;

        if !force && current_usage <= threshold {
            return Ok(());
        }

        // Target: free up ~30% of max tokens worth of history
        let target_tokens = max_tokens.saturating_mul(30) / 100;
        let min_turns = 2;
        let num_to_compact = self
            .context
            .oldest_turns_for_compaction(target_tokens, min_turns);

        if num_to_compact == 0 {
            return Ok(());
        }

        tracing::info!(
            "Compacting {} oldest turns (usage={}, threshold={})",
            num_to_compact,
            current_usage,
            threshold
        );

        if let Some(reason) = self.context.rule_based_compact(num_to_compact) {
            self.output.on_text(&format!("[System] {}\n", reason)).await;
            self.record_trace_event(
                TraceActor::Context,
                "context_compacted",
                TraceStatus::Ok,
                Some(reason.clone()),
                serde_json::json!({
                    "compacted_turns": num_to_compact,
                    "usage_tokens": current_usage as u64,
                    "threshold_tokens": threshold as u64,
                    "history_tokens": current_usage as u64,
                }),
                self.turn_span_id(),
                None,
            );
        }

        Ok(())
    }

    #[cfg(test)]
    async fn process_streaming_text(
        &self,
        full_text: &str,
        processed_idx: &mut usize,
        in_think_block: &mut bool,
    ) {
        loop {
            let remaining = &full_text[*processed_idx..];
            if remaining.is_empty() {
                break;
            }

            if *in_think_block {
                if let Some(end_idx) = remaining.find("</think>") {
                    let content = &remaining[..end_idx];
                    if !content.is_empty() {
                        self.output.on_thinking(content).await;
                    }
                    *processed_idx += end_idx + 8;
                    *in_think_block = false;
                } else {
                    // Check if we have a partial tag at the end
                    let potential_tag_start = remaining.rfind("</");
                    let len_to_process = if let Some(pos) = potential_tag_start {
                        pos
                    } else {
                        remaining.len()
                    };

                    if len_to_process > 0 {
                        self.output.on_thinking(&remaining[..len_to_process]).await;
                        *processed_idx += len_to_process;
                    }
                    break;
                }
            } else {
                // Strip <final> and </final> tags — render their content as plain text
                if let Some(start_idx) = remaining.find("<final>") {
                    let before = &remaining[..start_idx];
                    if !before.is_empty() {
                        self.output.on_text(before).await;
                    }
                    *processed_idx += start_idx + 7; // len("<final>")
                    continue;
                }
                if let Some(end_idx) = remaining.find("</final>") {
                    let before = &remaining[..end_idx];
                    if !before.is_empty() {
                        self.output.on_text(before).await;
                    }
                    *processed_idx += end_idx + 8; // len("</final>")
                    continue;
                }

                if let Some(start_idx) = remaining.find("<think>") {
                    let content = &remaining[..start_idx];
                    if !content.is_empty() {
                        self.output.on_text(content).await;
                    }
                    *processed_idx += start_idx + 7;
                    *in_think_block = true;
                } else {
                    // Check for partial <think> or <final> tag
                    let potential_tag_start = remaining.rfind('<');
                    let len_to_process = if let Some(pos) = potential_tag_start {
                        pos
                    } else {
                        remaining.len()
                    };

                    if len_to_process > 0 {
                        self.output.on_text(&remaining[..len_to_process]).await;
                        *processed_idx += len_to_process;
                    }
                    break;
                }
            }
        }
    }

    pub async fn step(
        &mut self,
        goal: String,
    ) -> Result<RunExit, Box<dyn std::error::Error + Send + Sync>> {
        let goal = goal.trim().to_string();
        if goal.is_empty() {
            return Ok(RunExit::YieldedToUser);
        }

        // Subagents share the cancelled flag with the parent runtime;
        // resetting it here would swallow cancel signals from cancel_job().
        if self.is_subagent {
            if self.is_cancelled() {
                return Ok(RunExit::StoppedByUser);
            }
        } else {
            self.cancelled
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }

        if self.is_autopilot {
            self.autopilot_todos_completed_count = self.count_completed_todos();
        }
        self.context.take_snapshot();

        let mut task_state = TaskState {
            iterations: 0,
            energy_points: self.initial_energy_budget,
        };

        let mut compaction_checked = false;
        let mut consecutive_empty_responses = 0;

        let mut turn_goal = goal.clone();
        for ext in &self.extensions {
            match ext.before_turn_start(&turn_goal).await {
                crate::core::extensions::ExtensionDecision::Continue => {}
                crate::core::extensions::ExtensionDecision::Intercept { prompt_overlay } => {
                    if let Some(overlay) = prompt_overlay {
                        turn_goal = overlay;
                    }
                }
                crate::core::extensions::ExtensionDecision::Halt { message } => {
                    self.output.on_text(&message).await;
                    self.output.on_text("\n").await;
                    self.output.flush().await;
                    return Ok(RunExit::YieldedToUser);
                }
            }
        }

        self.context.start_turn(turn_goal.clone());

        let (mut state, mut c_ids) = self.initialize_task_state(&turn_goal);
        self.begin_trace_run(&turn_goal, state.task_id.clone());
        c_ids.run_id = self
            .active_trace
            .as_ref()
            .map(|active| active.base_ctx.run_id.clone());
        self.record_trace_event(
            TraceActor::Context,
            "context_snapshot_taken",
            TraceStatus::Ok,
            None,
            serde_json::json!({}),
            self.turn_span_id(),
            None,
        );
        self.telemetry.start_span("agent_step", c_ids.clone());

        let output_clone = Arc::clone(&self.output);
        let _spinner_guard = ScopeGuard::new(move || {
            output_clone.clear_waiting();
        });

        loop {
            if let Some(exit) = self
                .execute_iteration(
                    &mut task_state,
                    &mut compaction_checked,
                    &mut consecutive_empty_responses,
                    &mut state,
                )
                .await?
            {
                return Ok(exit);
            }
        }
    }
}

#[cfg(test)]
mod tests;
