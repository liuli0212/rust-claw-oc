use crate::context::{AgentContext, ContextDiff, FunctionResponse, Message, Part};
use crate::llm_client::{LlmClient, StreamEvent};
use crate::tools::Tool;
use async_trait::async_trait;
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
    llm: Arc<dyn LlmClient>,
    tools: Vec<Arc<dyn Tool>>,
    pub context: AgentContext,
    output: Arc<dyn AgentOutput>,
    telemetry: Arc<crate::telemetry::TelemetryExporter>,
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
        task_state_store: Arc<crate::task_state::TaskStateStore>,
    ) -> Self {
        Self {
            session_id,
            llm,
            tools,
            context,
            output,
            telemetry,
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
            || msg.contains("connection reset")
            || msg.contains("eof")
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

    async fn collect_stream_response(
        &mut self,
        messages: Vec<Message>,
        system: Option<Message>,
        current_tools: Vec<Arc<dyn Tool>>,
    ) -> Result<StreamCollectionOutcome, Box<dyn std::error::Error + Send + Sync>> {
        let mut llm_attempts = 0;
        let mut tool_calls_accumulated: Vec<ToolCallRecord> = Vec::new();

        let full_text = loop {
            llm_attempts += 1;

            let stream_res = tokio::select! {
                res = self.llm.stream(messages.clone(), system.clone(), current_tools.clone()) => res,
                _ = self.cancel_token.notified() => {
                    self.output.flush().await;
                    self.context.end_turn();
                    return Ok(StreamCollectionOutcome::Exit(RunExit::StoppedByUser));
                }
            };

            match stream_res {
                Ok(mut rx) => {
                    let mut current_turn_text = String::new();

                    let stream_loop_res = loop {
                        tokio::select! {
                            event = rx.recv() => {
                                match event {
                                    Some(StreamEvent::Text(t)) => {
                                        current_turn_text.push_str(&t);
                                    }
                                    Some(StreamEvent::Thought(t)) => {
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
                        return Ok(StreamCollectionOutcome::Exit(exit));
                    }

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

        Ok(StreamCollectionOutcome::Completed {
            full_text,
            tool_calls: tool_calls_accumulated,
        })
    }

    fn initialize_task_state(
        &self,
        goal: &str,
    ) -> (
        crate::task_state::TaskStateSnapshot,
        crate::schema::CorrelationIds,
    ) {
        let mut state = self
            .task_state_store
            .load()
            .unwrap_or_else(|_| crate::task_state::TaskStateSnapshot::empty());

        if state.status == "in_progress" || state.status == "finished" || state.status == "failed" {
            tracing::info!(
                "Starting new task: cleaning previous {} task state",
                state.status
            );
            state = crate::task_state::TaskStateSnapshot::empty();
        }

        let current_task_id = state
            .task_id
            .clone()
            .unwrap_or_else(|| format!("tsk_{}", uuid::Uuid::new_v4().simple()));

        if state.status == "initialized" || state.status == "empty" {
            state.task_id = Some(current_task_id.clone());
            state.status = "in_progress".to_string();
            state.goal = Some(goal.to_string());
            let _ = self.task_state_store.save(&state);
        }

        let correlation_ids = crate::schema::CorrelationIds {
            session_id: self.session_id.clone(),
            task_id: Some(current_task_id.clone()),
            turn_id: self
                .context
                .current_turn
                .as_ref()
                .map(|turn| turn.turn_id.clone()),
            event_id: None,
        };

        (state, correlation_ids)
    }

    fn parse_tool_envelope(result: &str) -> Option<crate::tools::protocol::ToolExecutionEnvelope> {
        serde_json::from_str(result).ok()
    }

    fn extract_finish_task_summary_from_result(result: &str) -> Option<String> {
        Self::parse_tool_envelope(result).and_then(|envelope| envelope.finish_task_summary)
    }

    fn build_function_response_part(
        name: String,
        id: Option<String>,
        response: serde_json::Value,
        thought_signature: Option<String>,
    ) -> Part {
        Part {
            text: None,
            function_call: None,
            function_response: Some(FunctionResponse { name, response, id }),
            thought_signature,
            file_data: None,
        }
    }

    fn extract_tool_thought(call: &mut crate::context::FunctionCall) -> Option<String> {
        call.args
            .as_object_mut()
            .and_then(|obj| obj.remove("thought"))
            .and_then(|thought| thought.as_str().map(|value| value.to_string()))
            .filter(|thought| !thought.is_empty())
    }

    fn load_current_tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut current_tools = self.tools.clone();
        for skill in crate::skills::load_skills("skills") {
            current_tools.push(Arc::new(skill));
        }
        current_tools
    }

    async fn record_model_turn_and_maybe_yield(
        &mut self,
        full_text: &str,
        tool_calls_accumulated: &[ToolCallRecord],
    ) -> Option<RunExit> {
        let mut parts = Vec::new();
        if !full_text.is_empty() {
            parts.push(Part {
                text: Some(full_text.to_string()),
                function_call: None,
                function_response: None,
                thought_signature: None,
                file_data: None,
            });
        }
        for (tc, sig) in tool_calls_accumulated {
            parts.push(Part {
                text: None,
                function_call: Some(tc.clone()),
                function_response: None,
                thought_signature: sig.clone(),
                file_data: None,
            });
        }

        self.context.add_message_to_current_turn(Message {
            role: "model".to_string(),
            parts,
        });

        let text_without_think = Self::strip_think_blocks(full_text);
        let trimmed_clean = text_without_think.trim();

        if !trimmed_clean.is_empty() {
            self.output.on_text(trimmed_clean).await;
            self.output.on_text("\n").await;
        }

        if tool_calls_accumulated.is_empty() {
            self.output.flush().await;
            self.context.end_turn();
            self.telemetry.end_span("agent_step");
            return Some(RunExit::YieldedToUser);
        }

        None
    }

    async fn finalize_exit(&mut self, exit: RunExit, end_span: bool) -> RunExit {
        self.output.flush().await;
        self.context.end_turn();
        if end_span {
            self.telemetry.end_span("agent_step");
        }
        exit
    }

    async fn finalize_finished_run(&mut self, summary: String) -> RunExit {
        self.context.end_turn();
        self.telemetry.end_span("agent_step");
        RunExit::Finished(summary)
    }

    async fn check_loop_guards(&mut self, task_state: &mut TaskState) -> Option<RunExit> {
        if self.is_cancelled() {
            return Some(self.finalize_exit(RunExit::StoppedByUser, true).await);
        }

        if task_state.iterations >= Self::MAX_ITERATIONS {
            tracing::warn!(
                "Agent loop reached MAX_ITERATIONS ({}). Exiting to prevent runaway loops.",
                Self::MAX_ITERATIONS
            );
            return Some(
                self.finalize_exit(RunExit::AgentTurnLimitReached, false)
                    .await,
            );
        }

        task_state.iterations += 1;
        task_state.energy_points = task_state.energy_points.saturating_sub(1);

        if task_state.energy_points == 0 {
            tracing::error!("Energy points depleted.");
            self.output
                .on_text("[System] Energy depleted. Stopping to prevent infinite loops.")
                .await;
            return Some(
                self.finalize_exit(
                    RunExit::CriticallyFailed("Energy depleted".to_string()),
                    false,
                )
                .await,
            );
        }

        None
    }

    async fn collect_iteration_response(
        &mut self,
        state: &crate::task_state::TaskStateSnapshot,
        current_tools: &[Arc<dyn Tool>],
    ) -> Result<StreamCollectionOutcome, Box<dyn std::error::Error + Send + Sync>> {
        let max_tokens = self.context.max_history_tokens;
        let assembler = crate::context_assembler::ContextAssembler::new(max_tokens);
        let (messages, system, _) = self.context.build_llm_payload(state, &assembler);

        self.collect_stream_response(messages, system, current_tools.to_vec())
            .await
    }

    async fn handle_empty_iteration_response(
        &mut self,
        full_text: &str,
        tool_calls_accumulated: &[ToolCallRecord],
        consecutive_empty_responses: &mut usize,
    ) -> Option<RunExit> {
        if full_text.trim().is_empty() && tool_calls_accumulated.is_empty() {
            *consecutive_empty_responses += 1;
            if *consecutive_empty_responses >= Self::MAX_CONSECUTIVE_EMPTY_RESPONSES {
                return Some(
                    self.finalize_exit(
                        RunExit::CriticallyFailed("Too many empty responses".to_string()),
                        false,
                    )
                    .await,
                );
            }
            return Some(RunExit::RecoverableFailed(
                "Empty iteration response".to_string(),
            ));
        }

        *consecutive_empty_responses = 0;
        None
    }

    async fn dispatch_tool_call(
        &self,
        call: &crate::context::FunctionCall,
        current_tools: &[Arc<dyn Tool>],
    ) -> ToolDispatchOutcome {
        let tool_opt = current_tools.iter().find(|tool| tool.name() == call.name);

        if let Some(tool) = tool_opt {
            self.output
                .on_tool_start(&call.name, &call.args.to_string())
                .await;

            let (result, is_error, stopped) = tokio::select! {
                exec_res = tokio::time::timeout(
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
            };

            return ToolDispatchOutcome {
                result,
                is_error,
                stopped,
            };
        }

        ToolDispatchOutcome {
            result: format!("Tool not found: {}", call.name),
            is_error: true,
            stopped: false,
        }
    }

    async fn handle_successful_tool_effects(&mut self, result: &str) {
        let Some(envelope) = Self::parse_tool_envelope(result) else {
            return;
        };

        if let Some(path) = envelope.file_path.as_deref() {
            self.output.on_file(path).await;
        }

        if let (Some(kind), Some(source_path), Some(summary)) = (
            envelope.evidence_kind.as_deref(),
            envelope.evidence_source_path.as_deref(),
            envelope.evidence_summary.as_deref(),
        ) {
            let evidence = crate::evidence::Evidence::new(
                format!("{}_{}", kind, uuid::Uuid::new_v4().simple()),
                kind.to_string(),
                source_path.to_string(),
                1.0,
                summary.to_string(),
                envelope.output.clone(),
            );

            if kind == "directory" || kind == "file" {
                self.context.active_evidence.retain(|existing| {
                    existing.source_kind != kind || existing.source_path != source_path
                });
            } else if kind == "diagnostic" {
                self.context
                    .active_evidence
                    .retain(|existing| existing.source_kind != kind);
            }

            self.context.active_evidence.push(evidence);
        }

        if envelope.invalidate_diagnostic_evidence {
            for evidence in &mut self.context.active_evidence {
                if evidence.source_kind == "diagnostic" {
                    evidence.source_version = Some("invalidated_by_write".to_string());
                }
            }
        }
    }

    async fn execute_tool_round(
        &mut self,
        tool_calls_accumulated: Vec<ToolCallRecord>,
        current_tools: &[Arc<dyn Tool>],
        state: &mut crate::task_state::TaskStateSnapshot,
    ) -> Vec<Part> {
        let mut skip_remaining = false;
        let mut response_parts = Vec::new();

        for (mut call, thought_sig) in tool_calls_accumulated {
            if skip_remaining {
                response_parts.push(Self::build_function_response_part(
                    call.name.clone(),
                    call.id.clone(),
                    serde_json::json!({ "result": "Execution skipped as turn was interrupted." }),
                    thought_sig.clone(),
                ));
                continue;
            }
            if let Some(thought_str) = Self::extract_tool_thought(&mut call) {
                self.output.on_thinking(&thought_str).await;
                self.output.on_thinking("\n").await;
            }

            if call.name.trim().is_empty() {
                response_parts.push(Self::build_function_response_part(
                    "unknown".to_string(),
                    call.id.clone(),
                    serde_json::json!({ "result": "Error: Empty tool name" }),
                    thought_sig.clone(),
                ));
                continue;
            }

            let ToolDispatchOutcome {
                result,
                is_error,
                stopped,
            } = self.dispatch_tool_call(&call, current_tools).await;

            if stopped {
                self.output.on_error(&result).await;
                response_parts.push(Self::build_function_response_part(
                    call.name.clone(),
                    call.id.clone(),
                    serde_json::json!({ "result": result }),
                    thought_sig.clone(),
                ));
                skip_remaining = true;
                continue;
            }

            if is_error {
                self.output.on_error(&result).await;
            } else {
                self.output.on_tool_end(&result).await;
                self.handle_successful_tool_effects(&result).await;
                if let Some(summary) = Self::extract_finish_task_summary_from_result(&result) {
                    state.status = "finished".to_string();
                    let _ = self.task_state_store.save(state);
                    self.output.on_task_finish(&summary).await;
                }
            }

            response_parts.push(Part {
                ..Self::build_function_response_part(
                    call.name.clone(),
                    call.id.clone(),
                    serde_json::json!({ "result": result }),
                    thought_sig,
                )
            });
        }

        response_parts
    }

    async fn reconcile_after_tool_calls(
        &mut self,
        state_before_tools: &crate::task_state::TaskStateSnapshot,
    ) -> crate::task_state::TaskStateSnapshot {
        let state_after_tools = self
            .task_state_store
            .load()
            .unwrap_or_else(|_| crate::task_state::TaskStateSnapshot::empty());
        if state_before_tools != &state_after_tools {
            self.output.on_plan_update(&state_after_tools).await;
        }

        let compressed = self.context.compress_current_turn(400 * 1024);
        let truncated = self.context.truncate_current_turn_tool_results(30000);

        if compressed > 0 || truncated > 0 {
            let current_turn_id = self
                .context
                .current_turn
                .as_ref()
                .map(|turn| turn.turn_id.clone())
                .unwrap_or_else(|| "unknown".to_string());
            tracing::info!(
                "Turn {} compression: {} compressed, {} truncated.",
                current_turn_id,
                compressed,
                truncated
            );
        }

        state_after_tools
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
mod tests {
    use super::*;
    use crate::context::Message;
    use crate::llm_client::LlmError;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    struct TestLlmClient {
        stream_calls: AtomicUsize,
        stream_delay_ms: u64,
    }

    impl TestLlmClient {
        fn new() -> Self {
            Self {
                stream_calls: AtomicUsize::new(0),
                stream_delay_ms: 0,
            }
        }

        fn new_with_delay(stream_delay_ms: u64) -> Self {
            Self {
                stream_calls: AtomicUsize::new(0),
                stream_delay_ms,
            }
        }

        fn stream_call_count(&self) -> usize {
            self.stream_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl LlmClient for TestLlmClient {
        fn model_name(&self) -> &str {
            "test-model"
        }

        fn provider_name(&self) -> &str {
            "test-provider"
        }

        async fn stream(
            &self,
            _messages: Vec<Message>,
            _system_instruction: Option<Message>,
            _tools: Vec<Arc<dyn Tool>>,
        ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
            if self.stream_delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.stream_delay_ms)).await;
            }
            self.stream_calls.fetch_add(1, Ordering::SeqCst);
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        }
    }

    #[derive(Default)]
    struct OutputState {
        text: String,
        thinking: String,
    }

    struct TestOutput {
        state: Mutex<OutputState>,
    }

    impl TestOutput {
        fn new() -> Self {
            Self {
                state: Mutex::new(OutputState::default()),
            }
        }

        fn snapshot(&self) -> (String, String) {
            let state = self.state.lock().unwrap();
            (state.text.clone(), state.thinking.clone())
        }
    }

    #[async_trait]
    impl AgentOutput for TestOutput {
        async fn on_text(&self, text: &str) {
            self.state.lock().unwrap().text.push_str(text);
        }

        async fn on_thinking(&self, text: &str) {
            self.state.lock().unwrap().thinking.push_str(text);
        }

        async fn on_tool_start(&self, _name: &str, _args: &str) {}

        async fn on_tool_end(&self, _result: &str) {}

        async fn on_error(&self, _error: &str) {}
    }

    fn cleanup_session(session_id: &str) {
        let session_dir = crate::schema::StoragePaths::session_dir(session_id);
        let _ = std::fs::remove_dir_all(session_dir);
    }

    fn make_agent_loop(
        output: Arc<TestOutput>,
        llm: Arc<TestLlmClient>,
        session_id: &str,
    ) -> AgentLoop {
        let (telemetry, _handle) = crate::telemetry::TelemetryExporter::new();
        AgentLoop::new(
            session_id.to_string(),
            llm,
            Vec::new(),
            AgentContext::new(),
            output,
            Arc::new(telemetry),
            Arc::new(crate::task_state::TaskStateStore::new(session_id)),
        )
    }

    #[test]
    fn test_run_exit_label_matches_public_status_names() {
        assert_eq!(RunExit::Finished("done".to_string()).label(), "finished");
        assert_eq!(RunExit::StoppedByUser.label(), "stopped_by_user");
        assert_eq!(RunExit::AgentTurnLimitReached.label(), "turn_limit_reached");
        assert_eq!(RunExit::YieldedToUser.label(), "yielded_to_user");
        assert_eq!(
            RunExit::RecoverableFailed("retry".to_string()).label(),
            "recoverable_failed"
        );
        assert_eq!(
            RunExit::CriticallyFailed("boom".to_string()).label(),
            "critically_failed"
        );
    }

    #[test]
    fn test_strip_think_blocks_removes_closed_and_unclosed_blocks() {
        assert_eq!(
            AgentLoop::strip_think_blocks("hello<think>secret</think>world"),
            "helloworld"
        );
        assert_eq!(
            AgentLoop::strip_think_blocks("visible<think>hidden forever"),
            "visible"
        );
    }

    #[test]
    fn test_is_transient_llm_error_matches_retryable_signals_only() {
        assert!(AgentLoop::is_transient_llm_error(&LlmError::ApiError(
            "HTTP 503 upstream timeout".to_string()
        )));
        assert!(AgentLoop::is_transient_llm_error(&LlmError::ApiError(
            "connection reset by peer".to_string()
        )));
        assert!(!AgentLoop::is_transient_llm_error(&LlmError::ApiError(
            "invalid API key".to_string()
        )));
    }

    #[tokio::test]
    async fn test_process_streaming_text_routes_visible_and_thinking_segments() {
        let output = Arc::new(TestOutput::new());
        let llm = Arc::new(TestLlmClient::new());
        let session_id = "test-streaming-text";
        cleanup_session(session_id);
        let agent = make_agent_loop(output.clone(), llm, session_id);
        let mut processed_idx = 0;
        let mut in_think_block = false;
        let mut full_text = "Visible <think>internal".to_string();

        agent
            .process_streaming_text(&full_text, &mut processed_idx, &mut in_think_block)
            .await;

        full_text.push_str(" reasoning</think> done <final>answer</final>");
        agent
            .process_streaming_text(&full_text, &mut processed_idx, &mut in_think_block)
            .await;

        let (text, thinking) = output.snapshot();
        assert_eq!(text, "Visible  done answer");
        assert_eq!(thinking, "internal reasoning");
        assert_eq!(processed_idx, full_text.len());
        assert!(!in_think_block);
        cleanup_session(session_id);
    }

    #[tokio::test]
    async fn test_step_with_empty_goal_yields_without_starting_turn_or_llm() {
        let output = Arc::new(TestOutput::new());
        let llm = Arc::new(TestLlmClient::new());
        let session_id = "test-empty-goal";
        cleanup_session(session_id);
        let mut agent = make_agent_loop(output, llm.clone(), session_id);

        let exit = agent.step("   ".to_string()).await.unwrap();

        assert_eq!(exit, RunExit::YieldedToUser);
        assert!(agent.context.current_turn.is_none());
        assert_eq!(llm.stream_call_count(), 0);
        assert!(!crate::schema::StoragePaths::task_state_file(session_id).exists());
        cleanup_session(session_id);
    }

    #[tokio::test]
    async fn test_step_honors_cancel_during_pending_llm_stream_start() {
        let output = Arc::new(TestOutput::new());
        let llm = Arc::new(TestLlmClient::new_with_delay(200));
        let session_id = "test-cancel-before-stream";
        cleanup_session(session_id);
        let mut agent = make_agent_loop(output, llm.clone(), session_id);
        let cancel_token = agent.cancel_token.clone();
        let cancelled = agent.cancelled.clone();

        let step_handle =
            tokio::spawn(async move { agent.step("Refactor the core loop".to_string()).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancelled.store(true, Ordering::SeqCst);
        cancel_token.notify_waiters();

        let exit = step_handle.await.unwrap().unwrap();
        let store = crate::task_state::TaskStateStore::new(session_id);
        let stored_state = store.load().unwrap();

        assert_eq!(exit, RunExit::StoppedByUser);
        assert_eq!(llm.stream_call_count(), 0);
        assert_eq!(stored_state.status, "in_progress");
        assert_eq!(stored_state.goal.as_deref(), Some("Refactor the core loop"));
        cleanup_session(session_id);
    }
}
