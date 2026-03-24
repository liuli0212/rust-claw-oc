use crate::context::{AgentContext, ContextDiff, FunctionResponse, Message, Part};
use crate::llm_client::{LlmClient, StreamEvent};
use crate::tools::Tool;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

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
}

pub struct SilentOutputWrapper {
    pub inner: Arc<dyn AgentOutput>,
}

#[async_trait]
impl AgentOutput for SilentOutputWrapper {
    async fn on_waiting(&self, message: &str) { self.inner.on_waiting(message).await; }
    fn clear_waiting(&self) { self.inner.clear_waiting(); }
    async fn on_text(&self, text: &str) { self.inner.on_text(text).await; }
    async fn on_thinking(&self, _text: &str) {}
    async fn on_tool_start(&self, _name: &str, _args: &str) {}
    async fn on_tool_end(&self, _result: &str) {}
    async fn on_error(&self, error: &str) { self.inner.on_error(error).await; }
    async fn flush(&self) { self.inner.flush().await; }
    async fn on_file(&self, path: &str) { self.inner.on_file(path).await; }
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
}

pub trait OutputRouter: Send + Sync {
    fn try_route(&self, reply_to: &str) -> Option<Arc<dyn AgentOutput>>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum RunExit {
    Finished(String),
    StoppedByUser,
    AgentTurnLimitReached,
    YieldedToUser,
    RecoverableFailed(String),
    CriticallyFailed(String),
    AutopilotStalled(String),
}

impl RunExit {
    pub fn label(&self) -> &'static str {
        match self {
            RunExit::Finished(_) => "finished",
            RunExit::StoppedByUser => "stopped_by_user",
            RunExit::AgentTurnLimitReached => "turn_limit_reached",
            RunExit::YieldedToUser => "yielded_to_user",
            RunExit::RecoverableFailed(_) => "recoverable_failed",
            RunExit::CriticallyFailed(_) => "critically_failed",
            RunExit::AutopilotStalled(_) => "autopilot_stalled",
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

struct ToolDispatchOutcome {
    result: String,
    is_error: bool,
    stopped: bool,
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
    action_history: std::collections::VecDeque<String>,
    reflection_strike: u8,
    autopilot_todos_completed_count: usize,
    autopilot_work_dir: Option<PathBuf>,
}

impl AgentLoop {
    const MAX_LLM_RECOVERY_ATTEMPTS: usize = 3;
    const MAX_CONSECUTIVE_EMPTY_RESPONSES: usize = 3;
    const MAX_ITERATIONS: usize = 25;
    const INITIAL_ENERGY: usize = 100;

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
            action_history: std::collections::VecDeque::new(),
            reflection_strike: 0,
            autopilot_todos_completed_count: 0,
            autopilot_work_dir: None,
        }
    }

    pub fn request_cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.cancel_token.notify_waiters();
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
        // Include both base tools and dynamically loaded skills
        let mut all_tools = self.tools.clone();
        for skill in crate::skills::load_skills("skills") {
            all_tools.push(std::sync::Arc::new(skill));
        }

        serde_json::Value::Array(
            all_tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name(),
                        "description": t.description(),
                        "parameters": t.parameters_schema(),
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

        // Reset cancel flag at start of each step
        self.cancelled
            .store(false, std::sync::atomic::Ordering::SeqCst);
            
        if self.is_autopilot {
            self.autopilot_todos_completed_count = self.count_completed_todos();
        }
        self.context.take_snapshot();

        let mut task_state = TaskState {
            iterations: 0,
            energy_points: Self::INITIAL_ENERGY,
        };

        let mut compaction_checked = false;
        let mut consecutive_empty_responses = 0;

        self.context.start_turn(goal.clone());

        let (mut state, c_ids) = self.initialize_task_state(&goal);

        let _run_id = format!("run_{}", uuid::Uuid::new_v4().simple());
        self.telemetry.start_span("agent_step", c_ids.clone());

        let output_clone = Arc::clone(&self.output);
        let _spinner_guard = ScopeGuard::new(move || {
            output_clone.clear_waiting();
        });

        loop {
            if let Some(exit) = self.check_loop_guards(&mut task_state).await {
                return Ok(exit);
            }

            if !compaction_checked {
                let _ = self.maybe_compact_history(false).await;
                compaction_checked = true;
            }

            // Update status dashboard
            let (tokens, max_tokens, _, _, _) = self.context.get_context_status();
            self.output
                .on_status_update(
                    tokens,
                    max_tokens,
                    task_state.energy_points,
                    self.llm.provider_name(),
                    self.llm.model_name(),
                )
                .await;

            let current_tools = self.load_current_tools();

            let (full_text, tool_calls_accumulated) = match self
                .collect_iteration_response(&state, &current_tools)
                .await?
            {
                StreamCollectionOutcome::Completed {
                    full_text,
                    tool_calls,
                } => (full_text, tool_calls),
                StreamCollectionOutcome::Exit(exit) => return Ok(exit),
            };

            if let Some(exit) = self
                .handle_empty_iteration_response(
                    &full_text,
                    &tool_calls_accumulated,
                    &mut consecutive_empty_responses,
                )
                .await
            {
                if matches!(exit, RunExit::RecoverableFailed(_)) {
                    continue;
                }
                return Ok(exit);
            }

            if let Some(exit) = self
                .record_model_turn_and_maybe_yield(&full_text, &tool_calls_accumulated)
                .await
            {
                return Ok(exit);
            }

            let state_before_tools = state.clone();
            let response_parts = self
                .execute_tool_round(tool_calls_accumulated, &current_tools, &mut state)
                .await;

            if !response_parts.is_empty() {
                for part in &response_parts {
                    if let Some(res) = &part.function_response {
                        if res
                            .response
                            .get("signal")
                            .and_then(|s| s.as_str())
                            == Some("autopilot_meltdown")
                        {
                            return Ok(RunExit::AutopilotStalled(
                                "检测到深度死循环，反思无效，交还控制权".to_string(),
                            ));
                        }
                    }
                }

                self.context.add_message_to_current_turn(Message {
                    role: "function".to_string(),
                    parts: response_parts,
                });
            }

            if state.status == "finished" {
                let summary = state.summary();
                return Ok(self.finalize_finished_run(summary).await);
            }

            state = self.reconcile_after_tool_calls(&state_before_tools).await;
        }
    }
}

#[cfg(test)]
mod tests;
