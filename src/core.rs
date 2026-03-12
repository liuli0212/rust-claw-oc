#![allow(warnings)]
use crate::context::{AgentContext, ContextDiff, FunctionResponse, Message, Part};
use crate::llm_client::{LlmClient, StreamEvent};
use crate::tools::Tool;
use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

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
    async fn on_task_finish(&self, _summary: &str) {
        // Default: no-op
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RunExit {
    Finished(String),
    StoppedByUser,
    AgentTurnLimitReached,
    ContextLimitReached,
    YieldedToUser,
    RecoverableFailed(String),
    CriticallyFailed(String),
}

impl RunExit {
    pub fn label(&self) -> &'static str {
        match self {
            RunExit::Finished(_) => "finished",
            RunExit::StoppedByUser => "stopped_by_user",
            RunExit::AgentTurnLimitReached => "turn_limit_reached",
            RunExit::ContextLimitReached => "context_limit_reached",
            RunExit::YieldedToUser => "yielded_to_user",
            RunExit::RecoverableFailed(_) => "recoverable_failed",
            RunExit::CriticallyFailed(_) => "critically_failed",
        }
    }
}

struct TaskState {
    iterations: usize,
    energy_points: usize,
}

pub struct AgentLoop {
    session_id: String,
    llm: Arc<dyn LlmClient>,
    tools: Vec<Arc<dyn Tool>>,
    pub context: AgentContext,
    output: Arc<dyn AgentOutput>,
    telemetry: Arc<crate::telemetry::TelemetryExporter>,
    event_log: Arc<crate::event_log::EventLog>,
    task_state_store: Arc<crate::task_state::TaskStateStore>,
    pub cancel_token: Arc<Notify>,
    pub cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl AgentLoop {
    const MAX_LLM_RECOVERY_ATTEMPTS: usize = 3;
    const MAX_CONSECUTIVE_EMPTY_RESPONSES: usize = 3;
    const MAX_ITERATIONS: usize = 25;
    const INITIAL_ENERGY: usize = 100;

    pub fn new(
        session_id: String,
        llm: Arc<dyn LlmClient>,
        tools: Vec<Arc<dyn Tool>>,
        context: AgentContext,
        output: Arc<dyn AgentOutput>,
        telemetry: Arc<crate::telemetry::TelemetryExporter>,
        event_log: Arc<crate::event_log::EventLog>,
        task_state_store: Arc<crate::task_state::TaskStateStore>,
    ) -> Self {
        Self {
            session_id,
            llm,
            tools,
            context,
            output,
            telemetry,
            event_log,
            task_state_store,
            cancel_token: Arc::new(Notify::new()),
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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

    pub async fn force_compact(
        &mut self,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        self.maybe_compact_history(true).await?;
        Ok("Compaction triggered.".to_string())
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

    /// Strip `<think>...</think>` blocks from a string, returning only visible text.
    fn strip_think_blocks(text: &str) -> String {
        let mut s = text.to_string();
        while let Some(start) = s.find("<think>") {
            if let Some(end) = s.find("</think>") {
                s = format!("{}{}", &s[..start], &s[end + 8..]);
            } else {
                // Unclosed think block — strip from <think> to end
                s = s[..start].to_string();
                break;
            }
        }
        s
    }

    fn is_transient_llm_error(err: &crate::llm_client::LlmError) -> bool {
        let msg = format!("{}", err).to_lowercase();
        msg.contains("timeout")
            || msg.contains("500")
            || msg.contains("502")
            || msg.contains("503")
            || msg.contains("rate limit")
            || msg.contains("connection closed")
    }

    async fn handle_llm_error(&self, err: &crate::llm_client::LlmError, attempt: usize) -> bool {
        if Self::is_transient_llm_error(err) && attempt < Self::MAX_LLM_RECOVERY_ATTEMPTS {
            let exponent = (attempt as u32).min(6);
            let backoff_ms = 500u64.saturating_mul(2u64.pow(exponent));
            self.output
                .on_text(&format!(
                    "[System] Transient error. Retrying in {} ms... (Attempt {}/{})\n",
                    backoff_ms,
                    attempt,
                    Self::MAX_LLM_RECOVERY_ATTEMPTS
                ))
                .await;
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            return true;
        }
        false
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
        self.context.take_snapshot();

        let mut task_state = TaskState {
            iterations: 0,
            energy_points: Self::INITIAL_ENERGY,
        };

        let mut compaction_checked = false;
        let mut consecutive_empty_responses = 0;

        self.context.start_turn(goal.clone());

        // Init Task state — new task always clears old incomplete plans
        let mut state = self
            .task_state_store
            .load()
            .unwrap_or_else(|_| crate::task_state::TaskStateSnapshot::empty());

        if state.status == "in_progress" {
            // New user command arrived while old task still in progress — auto-clear
            tracing::info!("Auto-clearing previous in_progress task plan for new goal");
            state = crate::task_state::TaskStateSnapshot::empty();
        }

        let current_task_id = state
            .task_id
            .clone()
            .unwrap_or_else(|| format!("tsk_{}", uuid::Uuid::new_v4().simple()));

        if state.status == "initialized" || state.status == "empty" {
            state.task_id = Some(current_task_id.clone());
            state.status = "in_progress".to_string();
            state.goal = Some(goal.clone());
            let _ = self.task_state_store.save(&state);
        }

        let _run_id = format!("run_{}", uuid::Uuid::new_v4().simple());

        let c_ids = crate::schema::CorrelationIds {
            session_id: self.session_id.clone(),
            task_id: Some(current_task_id.clone()),
            turn_id: self
                .context
                .current_turn
                .as_ref()
                .map(|t| t.turn_id.clone()),
            event_id: None,
        };
        self.telemetry.start_span("agent_step", c_ids.clone());

        let output_clone = Arc::clone(&self.output);
        let _spinner_guard = ScopeGuard::new(move || {
            output_clone.clear_waiting();
        });

        loop {
            // Check persistent cancel flag at top of each iteration
            if self.is_cancelled() {
                self.output.flush().await;
                self.context.end_turn();
                self.telemetry.end_span("agent_step");
                return Ok(RunExit::StoppedByUser);
            }
            if task_state.iterations >= Self::MAX_ITERATIONS {
                tracing::warn!(
                    "Agent loop reached MAX_ITERATIONS ({}). Exiting to prevent runaway loops.",
                    Self::MAX_ITERATIONS
                );

                self.output.flush().await;
                self.context.end_turn();
                return Ok(RunExit::AgentTurnLimitReached);
            }
            task_state.iterations += 1;
            task_state.energy_points = task_state.energy_points.saturating_sub(1);

            if task_state.energy_points == 0 {
                tracing::error!("Energy points depleted.");
                self.output
                    .on_text("[System] Energy depleted. Stopping to prevent infinite loops.")
                    .await;

                self.output.flush().await;
                self.context.end_turn();
                return Ok(RunExit::CriticallyFailed("Energy depleted".to_string()));
            }

            if !compaction_checked {
                let _ = self.maybe_compact_history(false).await;
                compaction_checked = true;
            }

            // Dynamically load skills on every turn so we don't need to restart
            let mut current_tools = self.tools.clone();
            for skill in crate::skills::load_skills("skills") {
                current_tools.push(Arc::new(skill));
            }

            // Build cache-aware prompt using ContextAssembler
            // In a real advanced setup, ContextAssembler budget would be dynamic based on LLM size
            let max_tokens = self.context.max_history_tokens;
            let assembler = crate::context_assembler::ContextAssembler::new(max_tokens);
            let (messages, system, _) = self.context.build_llm_payload(&state, &assembler);

            let mut llm_attempts = 0;
            let mut tool_calls_accumulated: Vec<(crate::context::FunctionCall, Option<String>)> =
                Vec::new();

            let full_text = loop {
                llm_attempts += 1;

                let stream_res = tokio::select! {
                    res = self.llm.stream(messages.clone(), system.clone(), current_tools.clone()) => res,
                    _ = self.cancel_token.notified() => {
                        self.output.flush().await;
                        self.context.end_turn();
                        return Ok(RunExit::StoppedByUser);
                    }
                };

                match stream_res {
                    Ok(mut rx) => {
                        let mut current_turn_text = String::new();
                        // Text is fully buffered during streaming, then displayed
                        // after the stream completes. This ensures we can detect
                        // JSON blobs (like text-based finish_task) and handle them
                        // properly without partial display leaks.

                        let stream_loop_res = loop {
                            tokio::select! {
                                event = rx.recv() => {
                                    match event {
                                        Some(StreamEvent::Text(t)) => {
                                            // Buffer only — no display during stream
                                            current_turn_text.push_str(&t);
                                        }
                                        Some(StreamEvent::Thought(t)) => {
                                            // Thinking is still streamed in real-time
                                            self.output.on_thinking(&t).await;

                                            if !current_turn_text.ends_with("<think>") {
                                                current_turn_text.push_str("<think>");
                                            }
                                            current_turn_text.push_str(&t);
                                        }
                                        Some(StreamEvent::ToolCall(tc, sig)) => {
                                            tool_calls_accumulated.push((tc, sig));
                                        }
                                        Some(StreamEvent::Done) | None => break Ok(()),
                                        Some(StreamEvent::Error(e)) => {
                                            self.output.on_error(&format!("Stream error: {}", e)).await;
                                        }
                                    }
                                }
                                _ = self.cancel_token.notified() => {

                                    break Err(RunExit::StoppedByUser);
                                }
                            }
                        };

                        if let Err(exit) = stream_loop_res {
                            self.output.flush().await;
                            self.context.end_turn();
                            return Ok(exit);
                        }

                        // Close any unclosed think block in history
                        if current_turn_text.contains("<think>")
                            && !current_turn_text.contains("</think>")
                        {
                            current_turn_text.push_str("</think>");
                        }

                        break current_turn_text;
                    }
                    Err(e) => {
                        if !self.handle_llm_error(&e, llm_attempts).await {
                            self.output.flush().await;
                            return Err(Box::new(e));
                        }
                    }
                }
            };

            if full_text.trim().is_empty() && tool_calls_accumulated.is_empty() {
                consecutive_empty_responses += 1;
                if consecutive_empty_responses >= Self::MAX_CONSECUTIVE_EMPTY_RESPONSES {
                    self.output.flush().await;
                    self.context.end_turn();
                    return Ok(RunExit::CriticallyFailed(
                        "Too many empty responses".to_string(),
                    ));
                }
                continue;
            } else {
                consecutive_empty_responses = 0;
            }

            let mut parts = Vec::new();
            if !full_text.is_empty() {
                parts.push(Part {
                    text: Some(full_text.clone()),
                    function_call: None,
                    function_response: None,
                    thought_signature: None,
                });
            }
            for (tc, sig) in &tool_calls_accumulated {
                parts.push(Part {
                    text: None,
                    function_call: Some(tc.clone()),
                    function_response: None,
                    thought_signature: sig.clone(),
                });
            }

            self.context.add_message_to_current_turn(Message {
                role: "model".to_string(),
                parts,
            });

            // Post-stream text classification:
            // Check if the buffered text is a finish_task JSON blob streamed as
            // text content (some models ignore tool_choice: "required").
            let text_without_think = Self::strip_think_blocks(&full_text);
            let trimmed_clean = text_without_think.trim();
            let mut is_finish_task_fallback = false;

            if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed_clean) {
                if let Some(summary) = val
                    .as_object()
                    .and_then(|obj| obj.get("summary"))
                    .and_then(|v| v.as_str())
                {
                    tracing::info!("Detected text-based finish_task fallback, extracting summary");
                    self.output.on_task_finish(summary).await;
                    self.context.end_turn();
                    self.telemetry.end_span("agent_step");
                    return Ok(RunExit::Finished(summary.to_string()));
                }
            }

            // Normal text response — display it now (post-stream)
            // We display this even if there are tool calls, as it often contains
            // conversational context or explanations.
            if !trimmed_clean.is_empty() {
                self.output.on_text(trimmed_clean).await;
                self.output.on_text("\n").await;
            }

            if tool_calls_accumulated.is_empty() {
                self.output.flush().await;
                self.context.end_turn();
                self.telemetry.end_span("agent_step");
                return Ok(RunExit::YieldedToUser);
            }

            let state_before_tools = state.clone();
            let mut executed_signatures = HashSet::new();
            let mut stop_loop = false;

            let mut response_parts = Vec::new();
            for (mut call, thought_sig) in tool_calls_accumulated {
                if let Some(obj) = call.args.as_object_mut() {
                    if let Some(thought) = obj.remove("thought") {
                        if let Some(thought_str) = thought.as_str() {
                            if !thought_str.is_empty() {
                                self.output.on_thinking(thought_str).await;
                                self.output.on_thinking("\n").await;
                            }
                        }
                    }
                }

                let sig = format!("{}:{}", call.name, call.args);
                if !executed_signatures.insert(sig) || call.name.trim().is_empty() {
                    continue;
                }

                if call.name == "finish_task" {
                    let mut summary = call.args.to_string();

                    if let Some(obj) = call.args.as_object() {
                        if let Some(s) = obj.get("summary").and_then(|v| v.as_str()) {
                            summary = s.to_string();
                        }
                    } else if let Some(s) = call.args.as_str() {
                        // Handle double-encoded JSON strings from some models
                        if let Ok(inner) = serde_json::from_str::<serde_json::Value>(s) {
                            if let Some(obj) = inner.as_object() {
                                if let Some(inner_s) = obj.get("summary").and_then(|v| v.as_str()) {
                                    summary = inner_s.to_string();
                                }
                            }
                        }
                    }
                    self.context.end_turn();

                    self.output.on_task_finish(&summary).await;

                    self.telemetry.end_span("agent_step");
                    return Ok(RunExit::Finished(summary));
                }

                let tool_opt = self.tools.iter().find(|t| t.name() == call.name);
                let (result, is_error, stopped) = if let Some(tool) = tool_opt {
                    self.output
                        .on_tool_start(&call.name, &call.args.to_string())
                        .await;

                    tokio::select! {
                        exec_res = tokio::time::timeout(
                            // Default 120s timeout for any tool execution to prevent hanging
                            Duration::from_secs(120),
                            tool.execute(call.args.clone())
                        ) => {
                            match exec_res {
                        Ok(Ok(res)) => (res, false, false),
                        Ok(Err(e)) => (format!("Tool error: {}", e), true, false),
                        Err(e) => (format!("Timeout executing {}: {}", call.name, e), true, false),
                    }
                        }
                        _ = self.cancel_token.notified() => {
                            ("Tool execution interrupted by user.".to_string(), true, true)
                        }
                    }
                } else {
                    (format!("Tool not found: {}", call.name), true, false)
                };

                if stopped {
                    self.output.on_error(&result).await;
                    stop_loop = true;
                    // Even if stopped, we might want to record the partial result or just break
                    response_parts.push(Part {
                        text: None,
                        function_call: None,
                        function_response: Some(FunctionResponse {
                            name: call.name.clone(),
                            response: serde_json::json!({ "result": result }),
                            id: call.id.clone(),
                        }),
                        thought_signature: thought_sig,
                    });
                    break;
                }

                if is_error {
                    self.output.on_error(&result).await;
                } else {
                    self.output.on_tool_end(&result).await;
                    if call.name == "send_file" {
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&result) {
                            if let Some(path) = val.get("path").and_then(|v| v.as_str()) {
                                self.output.on_file(path).await;
                            }
                        }
                    } else if call.name == "read_file" {
                        if let Some(obj) = call.args.as_object() {
                            if let Some(path_val) = obj.get("path").and_then(|v| v.as_str()) {
                                let evidence_id = format!("file_{}", path_val);
                                let evidence = crate::evidence::Evidence::new(
                                    evidence_id.clone(),
                                    "file".to_string(),
                                    path_val.to_string(),
                                    1.0,
                                    format!("Direct read of {}", path_val),
                                    result.clone(),
                                );
                                // Maintain a clean state: remove older versions of the same file
                                self.context.active_evidence.retain(|e| {
                                    e.source_kind != "file" || e.source_path != path_val
                                });
                                self.context.active_evidence.push(evidence);
                            }
                        }
                    }
                }

                let final_result = result.to_string();

                if !is_error {
                    if call.name == "execute_bash" {
                        if let Some(obj) = call.args.as_object() {
                            if let Some(cmd) = obj.get("command").and_then(|v| v.as_str()) {
                                let cmd_trim = cmd.trim();
                                let is_diagnostic = cmd_trim.contains("cargo ")
                                    || cmd_trim.contains("npm run")
                                    || cmd_trim.contains("pytest")
                                    || cmd_trim.contains("tsc")
                                    || cmd_trim.contains("make");
                                let is_dir_list = cmd_trim.starts_with("ls ")
                                    || cmd_trim == "ls"
                                    || cmd_trim.starts_with("tree ")
                                    || cmd_trim == "tree"
                                    || cmd_trim.starts_with("find ");

                                if is_diagnostic || is_dir_list {
                                    let kind = if is_diagnostic {
                                        "diagnostic"
                                    } else {
                                        "directory"
                                    };
                                    let source_path = if is_diagnostic {
                                        "workspace_state"
                                    } else {
                                        cmd_trim
                                    };
                                    let evidence_id =
                                        format!("{}_{}", kind, uuid::Uuid::new_v4().simple());

                                    let evidence = crate::evidence::Evidence::new(
                                        evidence_id.clone(),
                                        kind.to_string(),
                                        source_path.to_string(),
                                        1.0,
                                        format!("Bash snapshot: {}", cmd_trim)
                                            .chars()
                                            .take(200)
                                            .collect(),
                                        final_result.clone(),
                                    );

                                    if is_dir_list {
                                        self.context.active_evidence.retain(|e| {
                                            e.source_kind != kind || e.source_path != source_path
                                        });
                                    } else {
                                        self.context
                                            .active_evidence
                                            .retain(|e| e.source_kind != kind);
                                    }

                                    self.context.active_evidence.push(evidence);
                                }
                            }
                        }
                    } else if call.name == "write_file" || call.name == "patch_file" {
                        // Invalidate diagnostic evidence because source code changed
                        for ev in self.context.active_evidence.iter_mut() {
                            if ev.source_kind == "diagnostic" {
                                ev.source_version = Some("invalidated_by_write".to_string());
                            }
                        }
                    }
                }

                response_parts.push(Part {
                    text: None,
                    function_call: None,
                    function_response: Some(FunctionResponse {
                        name: call.name.clone(),
                        response: serde_json::json!({ "result": final_result }),
                        id: call.id.clone(),
                    }),
                    thought_signature: thought_sig,
                });
            }

            if !response_parts.is_empty() {
                self.context.add_message_to_current_turn(Message {
                    role: "function".to_string(),
                    parts: response_parts,
                });
            }

            let state_after_tools = self
                .task_state_store
                .load()
                .unwrap_or_else(|_| crate::task_state::TaskStateSnapshot::empty());
            if state_before_tools != state_after_tools {
                self.output.on_plan_update(&state_after_tools).await;
            }
            state = state_after_tools;

            // Impose limits on extremely large tool results that might explode the context window
            let truncated = self.context.truncate_current_turn_tool_results(30000);
            if truncated > 0 {
                let current_turn_id = self
                    .context
                    .current_turn
                    .as_ref()
                    .map(|t| t.turn_id.as_str())
                    .unwrap_or("unknown");
                tracing::warn!("Turn {} tool results had {} oversized part(s) automatically truncated to save memory bounds.", current_turn_id, truncated);
            }

            if stop_loop {
                self.output.flush().await;
                self.context.end_turn();
                return Ok(RunExit::StoppedByUser);
            }
        }
    }
}
