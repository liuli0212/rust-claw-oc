use super::*;

impl AgentLoop {
    /// Strip `<think>...</think>` blocks from a string, returning only visible text.
    pub(super) fn strip_think_blocks(text: &str) -> String {
        let mut s = text.to_string();
        while let Some(start) = s.find("<think>") {
            if let Some(end) = s.find("</think>") {
                s = format!("{}{}", &s[..start], &s[end + 8..]);
            } else {
                s = s[..start].to_string();
                break;
            }
        }
        s
    }

    pub(super) fn is_transient_llm_error(err: &crate::llm_client::LlmError) -> bool {
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

    pub(super) async fn handle_llm_error(
        &self,
        err: &crate::llm_client::LlmError,
        attempt: usize,
    ) -> bool {
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

    pub(super) async fn collect_stream_response(
        &mut self,
        messages: Vec<Message>,
        system: Option<Message>,
        current_tools: Vec<Arc<dyn Tool>>,
        iteration_trace_ctx: Option<crate::trace::TraceContext>,
    ) -> Result<StreamCollectionOutcome, Box<dyn std::error::Error + Send + Sync>> {
        let mut llm_attempts = 0;
        let mut tool_calls_accumulated: Vec<ToolCallRecord> = Vec::new();

        let full_text = loop {
            llm_attempts += 1;

            let system_chars = system
                .as_ref()
                .map(|message| {
                    message
                        .parts
                        .iter()
                        .filter_map(|part| part.text.as_ref())
                        .map(|text| text.len())
                        .sum::<usize>()
                })
                .unwrap_or(0);
            let message_chars = messages
                .iter()
                .flat_map(|message| message.parts.iter())
                .filter_map(|part| part.text.as_ref())
                .map(|text| text.len())
                .sum::<usize>();
            let prompt_summary = format!(
                "System + {} messages, {} tools, ~{} chars",
                messages.len(),
                current_tools.len(),
                system_chars + message_chars
            );
            self.output.on_llm_request(&prompt_summary).await;
            let llm_span = iteration_trace_ctx.as_ref().map(|ctx| {
                self.trace_bus.start_span(
                    ctx,
                    TraceActor::Llm,
                    "llm_request_started",
                    serde_json::json!({
                        "provider": self.llm.provider_name(),
                        "model": self.llm.model_name(),
                        "message_count": messages.len(),
                        "tool_count": current_tools.len(),
                        "approx_prompt_chars": system_chars + message_chars,
                        "approx_prompt_tokens": ((system_chars + message_chars) / 4) as u64,
                        "stream_attempt": llm_attempts,
                    }),
                )
            });
            let llm_event_ctx = llm_span
                .as_ref()
                .map(TraceSpanHandle::child_context)
                .or_else(|| iteration_trace_ctx.clone());

            let stream_res = tokio::select! {
                res = self.llm.stream(messages.clone(), system.clone(), current_tools.clone()) => res,
                _ = self.cancel_token.notified() => {
                    if let Some(span) = llm_span {
                        span.finish(
                            "llm_request_finished",
                            TraceStatus::Cancelled,
                            Some("cancelled".to_string()),
                            serde_json::json!({}),
                        );
                    }
                    self.output.flush().await;
                    self.context.end_turn();
                    return Ok(StreamCollectionOutcome::Exit(RunExit::StoppedByUser));
                }
            };

            match stream_res {
                Ok(mut rx) => {
                    let mut current_turn_text = String::new();

                    let stream_loop_res: Result<(), crate::llm_client::LlmError> = loop {
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
                                        if let Some(ctx) = llm_event_ctx.as_ref() {
                                            self.trace_bus.record_event(
                                                ctx,
                                                TraceActor::Llm,
                                                "llm_tool_call_emitted",
                                                TraceStatus::Ok,
                                                Some(tc.name.clone()),
                                                serde_json::json!({
                                                    "tool_name": tc.name,
                                                    "args_preview": crate::context::AgentContext::truncate_chars(&tc.args.to_string(), 500),
                                                    "thought_signature": sig,
                                                }),
                                            );
                                        }
                                        tool_calls_accumulated.push((tc, sig));
                                    }
                                    Some(StreamEvent::Done) | None => break Ok(()),
                                    Some(StreamEvent::Error(e)) => {
                                        break Err(crate::llm_client::LlmError::ApiError(format!("Stream error: {}", e)));
                                    }
                                }
                            }
                            _ = self.cancel_token.notified() => {
                                if let Some(span) = llm_span {
                                    span.finish(
                                        "llm_request_finished",
                                        TraceStatus::Cancelled,
                                        Some("cancelled".to_string()),
                                        serde_json::json!({}),
                                    );
                                }
                                self.output.flush().await;
                                self.context.end_turn();
                                return Ok(StreamCollectionOutcome::Exit(RunExit::StoppedByUser));
                            }
                        }
                    };

                    match stream_loop_res {
                        Ok(()) => {
                            if current_turn_text.contains("<think>")
                                && !current_turn_text.contains("</think>")
                            {
                                current_turn_text.push_str("</think>");
                            }
                            if let Some(span) = llm_span {
                                span.finish(
                                    "llm_request_finished",
                                    TraceStatus::Ok,
                                    Some(crate::context::AgentContext::truncate_chars(
                                        &current_turn_text,
                                        240,
                                    )),
                                    serde_json::json!({
                                        "response_chars": current_turn_text.len(),
                                        "tool_calls": tool_calls_accumulated.len(),
                                    }),
                                );
                            }

                            break current_turn_text;
                        }
                        Err(e) => {
                            if let Some(span) = llm_span {
                                span.finish(
                                    "llm_request_finished",
                                    if Self::is_transient_llm_error(&e) {
                                        TraceStatus::Retrying
                                    } else {
                                        TraceStatus::Error
                                    },
                                    Some(crate::context::AgentContext::truncate_chars(
                                        &e.to_string(),
                                        240,
                                    )),
                                    serde_json::json!({
                                        "error": e.to_string(),
                                        "stream_attempt": llm_attempts,
                                    }),
                                );
                            }
                            if let Some(ctx) = iteration_trace_ctx.as_ref() {
                                self.trace_bus.record_event(
                                    ctx,
                                    TraceActor::Llm,
                                    if Self::is_transient_llm_error(&e) {
                                        "llm_retry_scheduled"
                                    } else {
                                        "llm_error"
                                    },
                                    if Self::is_transient_llm_error(&e) {
                                        TraceStatus::Retrying
                                    } else {
                                        TraceStatus::Error
                                    },
                                    Some(crate::context::AgentContext::truncate_chars(
                                        &e.to_string(),
                                        240,
                                    )),
                                    serde_json::json!({
                                        "error": e.to_string(),
                                        "stream_attempt": llm_attempts,
                                    }),
                                );
                            }
                            tool_calls_accumulated.clear(); // 清理避免重试时带有历史 tool_call
                            if !self.handle_llm_error(&e, llm_attempts).await {
                                self.output.flush().await;
                                return Err(Box::new(e));
                            }
                        }
                    }
                }
                Err(e) => {
                    if let Some(span) = llm_span {
                        span.finish(
                            "llm_request_finished",
                            if Self::is_transient_llm_error(&e) {
                                TraceStatus::Retrying
                            } else {
                                TraceStatus::Error
                            },
                            Some(crate::context::AgentContext::truncate_chars(
                                &e.to_string(),
                                240,
                            )),
                            serde_json::json!({
                                "error": e.to_string(),
                                "stream_attempt": llm_attempts,
                            }),
                        );
                    }
                    if let Some(ctx) = iteration_trace_ctx.as_ref() {
                        self.trace_bus.record_event(
                            ctx,
                            TraceActor::Llm,
                            if Self::is_transient_llm_error(&e) {
                                "llm_retry_scheduled"
                            } else {
                                "llm_error"
                            },
                            if Self::is_transient_llm_error(&e) {
                                TraceStatus::Retrying
                            } else {
                                TraceStatus::Error
                            },
                            Some(crate::context::AgentContext::truncate_chars(
                                &e.to_string(),
                                240,
                            )),
                            serde_json::json!({
                                "error": e.to_string(),
                                "stream_attempt": llm_attempts,
                            }),
                        );
                    }
                    if !self.handle_llm_error(&e, llm_attempts).await {
                        self.output.flush().await;
                        return Err(Box::new(e));
                    }
                }
            }
        };

        self.output.on_llm_response(&full_text).await;

        Ok(StreamCollectionOutcome::Completed {
            full_text,
            tool_calls: tool_calls_accumulated,
        })
    }

    pub(super) fn initialize_task_state(
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
            run_id: None,
            turn_id: self
                .context
                .current_turn
                .as_ref()
                .map(|turn| turn.turn_id.clone()),
            event_id: None,
        };

        (state, correlation_ids)
    }

    pub(super) fn parse_tool_envelope(
        result: &str,
    ) -> Option<crate::tools::protocol::ToolExecutionEnvelope> {
        crate::tools::protocol::ToolExecutionEnvelope::from_json_str(result)
    }

    pub(super) fn build_function_response_part(
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

    pub(super) fn extract_tool_thought(call: &mut crate::context::FunctionCall) -> Option<String> {
        call.args
            .as_object_mut()
            .and_then(|obj| obj.remove("thought"))
            .and_then(|thought| thought.as_str().map(|value| value.to_string()))
            .filter(|thought| !thought.is_empty())
    }

    pub(super) async fn load_current_tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut current_tools = self.tools.clone();

        // Extension hook: before_tool_resolution — let extensions filter the tool set
        for ext in &self.extensions {
            current_tools = ext.before_tool_resolution(current_tools).await;
        }

        if !self.llm.capabilities().supports_code_mode {
            current_tools.retain(|tool| {
                let name = tool.name();
                name != "exec" && name != "wait"
            });
        }

        current_tools
    }

    pub(super) async fn record_model_turn_and_maybe_yield(
        &mut self,
        full_text: &str,
        tool_calls_accumulated: &[ToolCallRecord],
        state: &mut crate::task_state::TaskStateSnapshot,
    ) -> Option<RunExit> {
        // ── Canary check BEFORE history insertion ──
        // If the LLM leaked the canary, we must prevent the leaked text from
        // entering the dialogue history (and thus being re-fed in later turns).
        // Check both the text body AND tool-call arguments — the model could
        // exfiltrate the canary through FunctionCall.args.
        let text_without_think = Self::strip_think_blocks(full_text);
        let trimmed_clean = text_without_think.trim();

        let args_leak = tool_calls_accumulated
            .iter()
            .any(|(tc, _)| crate::security::check_canary_leak(&tc.args.to_string()));

        if crate::security::check_canary_leak(full_text) || args_leak {
            tracing::warn!("Canary token leaked in LLM output — possible prompt extraction attack");
            self.output
                .on_text("[Security] Canary leak detected — suppressing output.\n")
                .await;
            // Record a redacted placeholder so turn pairing stays consistent.
            self.context.add_message_to_current_turn(Message {
                role: "model".to_string(),
                parts: vec![Part {
                    text: Some("[REDACTED — canary leak detected]".to_string()),
                    function_call: None,
                    function_response: None,
                    thought_signature: None,
                    file_data: None,
                }],
            });
            self.record_trace_event(
                TraceActor::System,
                "yielded_to_user",
                TraceStatus::Yielded,
                Some("Canary leak detected — output suppressed".to_string()),
                serde_json::json!({}),
                self.turn_span_id(),
                None,
            );
            self.output.flush().await;
            self.context.end_turn();
            self.telemetry.end_span("agent_step");
            return Some(RunExit::YieldedToUser);
        }

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

        if tool_calls_accumulated.is_empty() {
            if trimmed_clean.is_empty() {
                self.record_trace_event(
                    TraceActor::System,
                    "yielded_to_user",
                    TraceStatus::Yielded,
                    Some("No visible text or tool call was emitted".to_string()),
                    serde_json::json!({}),
                    self.turn_span_id(),
                    None,
                );
                self.output.flush().await;
                self.context.end_turn();
                self.telemetry.end_span("agent_step");
                return Some(RunExit::YieldedToUser);
            }

            let final_answer = trimmed_clean.to_string();
            if let Err(reason) = self.prepare_finished_run(&final_answer, state).await {
                self.output.on_text(&format!("[System] {}\n", reason)).await;
                self.context.add_message_to_current_turn(Message {
                    role: "user".to_string(),
                    parts: vec![Part {
                        text: Some(format!(
                            "[System] Completion was denied: {reason}. Continue working."
                        )),
                        function_call: None,
                        function_response: None,
                        thought_signature: None,
                        file_data: None,
                    }],
                });
                return None;
            }

            self.output.on_text(&final_answer).await;
            self.output.on_text("\n").await;
            return Some(self.finalize_finished_run(final_answer).await);
        }

        if !trimmed_clean.is_empty() {
            self.output.on_text(trimmed_clean).await;
            self.output.on_text("\n").await;
        }

        None
    }

    pub(super) async fn prepare_finished_run(
        &mut self,
        summary: &str,
        state: &mut crate::task_state::TaskStateSnapshot,
    ) -> Result<(), String> {
        if self.is_autopilot && self.has_uncompleted_todos() {
            return Err(
                "Action Denied. Autopilot 模式下必须完成 TODOS.md 中的所有任务才能结束。"
                    .to_string(),
            );
        }

        for ext in &self.extensions {
            if let crate::core::extensions::FinishDecision::Deny { reason } =
                ext.before_finish().await
            {
                tracing::warn!("Extension denied finish: {}", reason);
                return Err(reason);
            }
        }

        state.status = "finished".to_string();
        state.finish_summary = Some(summary.to_string());
        for step in &mut state.plan_steps {
            step.status = "completed".to_string();
        }
        state.current_step = None;
        let _ = self.task_state_store.save(state);

        for ext in &self.extensions {
            ext.on_finish_committed(summary).await;
        }

        Ok(())
    }

    pub(super) async fn finalize_exit(&mut self, exit: RunExit, end_span: bool) -> RunExit {
        self.output.flush().await;
        self.context.end_turn();
        if end_span {
            self.telemetry.end_span("agent_step");
        }
        let (end_name, status, summary) = match &exit {
            RunExit::Finished(summary) => ("run_finished", TraceStatus::Ok, Some(summary.clone())),
            RunExit::StoppedByUser => (
                "run_cancelled",
                TraceStatus::Cancelled,
                Some("stopped_by_user".to_string()),
            ),
            RunExit::YieldedToUser => (
                "run_finished",
                TraceStatus::Yielded,
                Some("yielded_to_user".to_string()),
            ),
            RunExit::RecoverableFailed(message)
            | RunExit::CriticallyFailed(message)
            | RunExit::AutopilotStalled(message)
            | RunExit::EnergyDepleted(message) => {
                ("run_failed", TraceStatus::Error, Some(message.clone()))
            }
        };
        self.finish_active_trace(end_name, status, summary);
        exit
    }

    pub(super) async fn finalize_finished_run(&mut self, summary: String) -> RunExit {
        self.output.flush().await;
        self.output.on_task_finish(&summary).await;
        self.output.flush().await;
        self.context.end_turn();
        self.telemetry.end_span("agent_step");
        self.finish_active_trace("run_finished", TraceStatus::Ok, Some(summary.clone()));
        RunExit::Finished(summary)
    }

    pub(super) async fn check_loop_guards(
        &mut self,
        task_state: &mut TaskState,
    ) -> Option<RunExit> {
        if self.is_cancelled() {
            return Some(self.finalize_exit(RunExit::StoppedByUser, true).await);
        }

        task_state.iterations += 1;
        task_state.energy_points = task_state.energy_points.saturating_sub(1);

        if task_state.energy_points == 0 {
            if self.is_autopilot {
                let current_completed = self.count_completed_todos();
                if current_completed > self.autopilot_todos_completed_count {
                    // Physical audit passed — generate status summary for user
                    tracing::info!(
                        "Autopilot physical audit passed. Resetting energy and generating status summary."
                    );
                    self.output
                        .on_text("[System] 物理审计通过，正在生成阶段性进展报告...\n")
                        .await;

                    let summary = self.generate_status_summary().await;
                    self.output.on_text(&format!("\n{}\n\n", summary)).await;

                    // Use rule_based_compact to safely compress history (preserves message pairing)
                    let keep_turns = 3;
                    let to_compact = self
                        .context
                        .dialogue_history
                        .len()
                        .saturating_sub(keep_turns);
                    if to_compact > 0 {
                        if let Some(reason) = self.context.rule_based_compact(to_compact) {
                            tracing::info!("Autopilot compaction: {}", reason);
                        }
                    }

                    task_state.energy_points = Self::INITIAL_ENERGY;
                    self.autopilot_todos_completed_count = current_completed;
                    return None; // Continue loop
                }
            }

            tracing::warn!("Energy points depleted. Generating status summary for user handoff.");
            self.output
                .on_text("[System] 能量耗尽，正在生成阶段性进展报告并暂停任务...\n")
                .await;
            self.record_trace_event(
                TraceActor::System,
                "energy_depleted",
                TraceStatus::Error,
                Some("Energy budget exhausted".to_string()),
                serde_json::json!({
                    "iterations": task_state.iterations,
                }),
                self.turn_span_id(),
                None,
            );

            let summary = self.generate_status_summary().await;

            return Some(
                self.finalize_exit(RunExit::EnergyDepleted(summary), true)
                    .await,
            );
        }

        None
    }

    pub(super) async fn collect_iteration_response(
        &mut self,
        state: &crate::task_state::TaskStateSnapshot,
        current_tools: &[Arc<dyn Tool>],
        iteration_trace_ctx: Option<crate::trace::TraceContext>,
    ) -> Result<StreamCollectionOutcome, Box<dyn std::error::Error + Send + Sync>> {
        // Extension hook: before_prompt_build — let extensions inject skill contract/instructions
        let mut draft = crate::core::extensions::PromptDraft::default();
        for ext in &self.extensions {
            draft = ext.before_prompt_build(draft).await;
        }

        // If an extension injected a skill contract, attach it to context for prompt assembly
        if let Some(contract) = &draft.skill_contract {
            self.context.skill_contract = Some(contract.clone());
        } else {
            self.context.skill_contract = None;
        }
        self.context.skill_instructions = draft.skill_instructions.clone();
        self.context.skill_state_summary = draft.skill_state_summary.clone();

        let mut execution_notices = draft.execution_notices;
        let code_mode_visible = self.llm.capabilities().supports_code_mode
            && current_tools
                .iter()
                .any(|tool| matches!(tool.name().as_str(), "exec" | "wait"));
        if code_mode_visible {
            let available_nested_tools: Vec<String> = current_tools
                .iter()
                .map(|tool| tool.name())
                .filter(|name| crate::tools::invocation::is_code_mode_nested_tool(name))
                .collect();
            let code_mode_notice = crate::code_mode::description::execution_notice(
                &available_nested_tools,
                self.code_mode_format,
            );
            execution_notices = Some(match execution_notices {
                Some(existing) if !existing.trim().is_empty() => {
                    format!("{existing}\n\n{code_mode_notice}")
                }
                _ => code_mode_notice,
            });
        }
        self.context.execution_notices = execution_notices;

        let max_tokens = self.context.max_history_tokens;
        let assembler = crate::context_assembler::ContextAssembler::new(max_tokens);
        let (messages, system, _) = self.context.build_llm_payload(state, &assembler);
        let llm_tools = self.llm_visible_tools_for_code_mode(current_tools);

        self.collect_stream_response(messages, system, llm_tools, iteration_trace_ctx)
            .await
    }

    pub(super) fn llm_visible_tools_for_code_mode(
        &self,
        current_tools: &[Arc<dyn Tool>],
    ) -> Vec<Arc<dyn Tool>> {
        if self.code_mode_format.exposes_function_exec() {
            return current_tools.to_vec();
        }

        current_tools
            .iter()
            .filter(|tool| tool.name() != "exec")
            .cloned()
            .collect()
    }

    pub(super) async fn handle_empty_iteration_response(
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

    pub(super) fn synthesize_text_exec_tool_call(
        &self,
        full_text: &str,
        tool_calls_accumulated: &[ToolCallRecord],
    ) -> Option<(String, Vec<ToolCallRecord>)> {
        if !self.code_mode_format.accepts_text_command() || !tool_calls_accumulated.is_empty() {
            return None;
        }

        let visible_text = Self::strip_think_blocks(full_text);
        let parsed = crate::code_mode::text_command::parse_text_exec_command(&visible_text)?;
        if parsed.code.trim().is_empty() {
            return None;
        }

        let mut args = serde_json::Map::new();
        args.insert("code".to_string(), serde_json::Value::String(parsed.code));
        if let Some(auto_flush_ms) = parsed.auto_flush_ms {
            args.insert(
                "auto_flush_ms".to_string(),
                serde_json::Value::Number(auto_flush_ms.into()),
            );
        }
        if let Some(cell_timeout_ms) = parsed.cell_timeout_ms {
            args.insert(
                "cell_timeout_ms".to_string(),
                serde_json::Value::Number(cell_timeout_ms.into()),
            );
        }

        let call = crate::context::FunctionCall {
            name: "exec".to_string(),
            args: serde_json::Value::Object(args),
            id: Some(format!("text_exec_{}", uuid::Uuid::new_v4().simple())),
        };

        Some((String::new(), vec![(call, None)]))
    }

    fn build_tool_executor_with_budget(
        &self,
        current_tools: &[Arc<dyn Tool>],
        step_budget: crate::tools::invocation::StepBudgetHandle,
    ) -> crate::tools::invocation::UnifiedToolExecutor {
        crate::tools::invocation::UnifiedToolExecutor::new(
            crate::tools::invocation::UnifiedToolExecutorConfig {
                current_tools: current_tools.to_vec(),
                visible_tools: current_tools.iter().map(|tool| tool.name()).collect(),
                extensions: self.extensions.clone(),
                session_id: self.session_id.clone(),
                reply_to: self.reply_to.clone(),
                step_budget,
                session_deadline: self.session_deadline,
                cancel_token: self.cancel_token.clone(),
                is_autopilot: self.is_autopilot,
                todos_path: self.todos_path(),
                execution_guard_state: self.execution_guard_state.clone(),
            },
        )
    }

    #[cfg(test)]
    pub(super) async fn dispatch_tool_call(
        &mut self,
        call: &crate::context::FunctionCall,
        current_tools: &[Arc<dyn Tool>],
        remaining_steps: usize,
        iteration_trace_ctx: Option<crate::trace::TraceContext>,
    ) -> ToolDispatchOutcome {
        let step_budget = crate::tools::invocation::StepBudgetHandle::new(remaining_steps);
        let tool_executor = self.build_tool_executor_with_budget(current_tools, step_budget);
        self.dispatch_tool_call_with_executor(
            call,
            current_tools,
            tool_executor,
            iteration_trace_ctx,
        )
        .await
    }

    pub(super) async fn dispatch_tool_call_with_executor(
        &mut self,
        call: &crate::context::FunctionCall,
        current_tools: &[Arc<dyn Tool>],
        tool_executor: crate::tools::invocation::UnifiedToolExecutor,
        iteration_trace_ctx: Option<crate::trace::TraceContext>,
    ) -> ToolDispatchOutcome {
        let tool_opt = current_tools.iter().find(|tool| tool.name() == call.name);

        if tool_opt.is_some() {
            tracing::info!("Executing tool '{}' with args: {}", call.name, call.args);
            self.output
                .on_tool_start(&call.name, &call.args.to_string())
                .await;
            let mut tool_span = iteration_trace_ctx.as_ref().map(|ctx| {
                self.trace_bus.start_span(
                    ctx,
                    TraceActor::Tool,
                    "tool_started",
                    serde_json::json!({
                        "tool_name": call.name,
                        "args_preview": crate::context::AgentContext::truncate_chars(&call.args.to_string(), 500),
                        "remaining_steps": tool_executor.remaining_steps(),
                        "timeout_sec": 120,
                    }),
                )
            });

            if self.is_cancelled() {
                tracing::warn!(
                    "Tool execution '{}' aborted before dispatch because the run was already cancelled",
                    call.name
                );
                if let Some(span) = tool_span.take() {
                    span.finish(
                        "tool_cancelled",
                        TraceStatus::Cancelled,
                        Some("Tool execution interrupted by user.".to_string()),
                        serde_json::json!({
                            "tool_name": call.name,
                        }),
                    );
                }
                return ToolDispatchOutcome {
                    result: "Tool execution interrupted by user.".to_string(),
                    is_error: true,
                    stopped: true,
                    guard_signal: None,
                };
            }

            if matches!(call.name.as_str(), "exec" | "wait") {
                let mut exec_outcome = crate::code_mode::entry::dispatch_tool_call(
                    call,
                    crate::code_mode::entry::CodeModeDispatchConfig {
                        current_tools: current_tools.to_vec(),
                        extensions: self.extensions.clone(),
                        service: self.code_mode_service.clone(),
                        session_id: self.session_id.clone(),
                        reply_to: self.reply_to.clone(),
                        remaining_steps: tool_executor.remaining_steps(),
                        session_deadline: self.session_deadline,
                        iteration_trace_ctx: iteration_trace_ctx.clone(),
                        parent_span_id: tool_span.as_ref().map(|span| span.span_id().to_string()),
                        trace_bus: self.trace_bus.clone(),
                        provider: self.llm.provider_name().to_string(),
                        model: self.llm.model_name().to_string(),
                        cancel_token: self.cancel_token.clone(),
                        output: self.output.clone(),
                        is_autopilot: self.is_autopilot,
                        todos_path: self.todos_path(),
                        execution_guard_state: self.execution_guard_state.clone(),
                    },
                )
                .await;
                if let Some(signal) = tool_executor.record_action_outcome(
                    &call.name,
                    &call.args,
                    exec_outcome.is_error,
                ) {
                    exec_outcome.result = signal.message().to_string();
                    exec_outcome.is_error = true;
                    exec_outcome.guard_signal = Some(signal);
                }
                if let Some(span) = tool_span.take() {
                    span.finish(
                        if exec_outcome.is_error {
                            "tool_failed"
                        } else {
                            "tool_finished"
                        },
                        if exec_outcome.is_error {
                            TraceStatus::Error
                        } else {
                            TraceStatus::Ok
                        },
                        Some(crate::context::AgentContext::truncate_chars(
                            &exec_outcome.result,
                            240,
                        )),
                        serde_json::json!({
                            "tool_name": call.name,
                            "result_preview": crate::context::AgentContext::truncate_chars(&exec_outcome.result, 500),
                        }),
                    );
                }
                return exec_outcome;
            }

            let outcome = tool_executor
                .execute(crate::tools::invocation::ToolExecutionRequest {
                    tool_name: call.name.clone(),
                    args: call.args.clone(),
                    origin: crate::tools::invocation::ToolCallOrigin::TopLevel,
                    timeout: Duration::from_secs(120),
                    trace_ctx: iteration_trace_ctx.clone(),
                    context_parent_span_id: tool_span
                        .as_ref()
                        .map(|span| span.span_id().to_string()),
                })
                .await;
            if let Some(span) = tool_span.take() {
                span.finish(
                    if outcome.stopped {
                        "tool_cancelled"
                    } else if outcome.is_error {
                        "tool_failed"
                    } else {
                        "tool_finished"
                    },
                    if outcome.stopped {
                        TraceStatus::Cancelled
                    } else if outcome.is_error {
                        TraceStatus::Error
                    } else {
                        TraceStatus::Ok
                    },
                    Some(crate::context::AgentContext::truncate_chars(
                        &outcome.result,
                        240,
                    )),
                    serde_json::json!({
                        "tool_name": call.name,
                        "result_preview": crate::context::AgentContext::truncate_chars(&outcome.result, 500),
                        "result_size_chars": outcome.result.len(),
                    }),
                );
            }

            return ToolDispatchOutcome {
                result: outcome.result,
                is_error: outcome.is_error,
                stopped: outcome.stopped,
                guard_signal: outcome.guard_signal,
            };
        }

        ToolDispatchOutcome {
            result: format!("Tool not found: {}", call.name),
            is_error: true,
            stopped: false,
            guard_signal: None,
        }
    }

    pub(super) async fn handle_successful_tool_effects(&mut self, result: &str) {
        let Some(envelope) = Self::parse_tool_envelope(result) else {
            return;
        };

        // Extension hook: after_tool_result — let extensions react to tool outputs
        for ext in &self.extensions {
            ext.after_tool_result(&envelope).await;
        }

        if let Some(path) = envelope.effects.file_path.as_deref() {
            self.output.on_file(path).await;
        }

        if let (Some(kind), Some(source_path), Some(summary)) = (
            envelope.effects.evidence_kind.as_deref(),
            envelope.effects.evidence_source_path.as_deref(),
            envelope.effects.evidence_summary.as_deref(),
        ) {
            let evidence = crate::evidence::Evidence::new(
                format!("{}_{}", kind, uuid::Uuid::new_v4().simple()),
                kind.to_string(),
                source_path.to_string(),
                1.0,
                summary.to_string(),
                envelope.result.output.clone(),
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

        if envelope.effects.invalidate_diagnostic_evidence {
            for evidence in &mut self.context.active_evidence {
                if evidence.source_kind == "diagnostic" {
                    evidence.source_version = Some("invalidated_by_write".to_string());
                }
            }
        }
    }

    fn format_user_prompt(request: &crate::tools::protocol::UserPromptRequest) -> String {
        let mut message = request.question.clone();
        if !request.options.is_empty() {
            message.push_str(&format!("\nOptions: {}", request.options.join(", ")));
        }
        if let Some(recommendation) = &request.recommendation {
            message.push_str(&format!("\nRecommended: {}", recommendation));
        }
        message
    }

    pub(super) async fn execute_tool_round(
        &mut self,
        tool_calls_accumulated: Vec<ToolCallRecord>,
        current_tools: &[Arc<dyn Tool>],
        iteration_trace_ctx: Option<crate::trace::TraceContext>,
        remaining_steps: usize,
    ) -> (Vec<Part>, bool) {
        let mut skip_remaining = false;
        let mut should_yield_to_user = false;
        let mut response_parts = Vec::new();

        let todos_before = if self.is_autopilot {
            self.count_todos_status()
        } else {
            (0, 0)
        };
        let step_budget = crate::tools::invocation::StepBudgetHandle::new(remaining_steps);
        let tool_executor = self.build_tool_executor_with_budget(current_tools, step_budget);

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

            if let Some(reason) = tool_executor.autopilot_denial_for_call(&call.name, &call.args) {
                response_parts.push(Self::build_function_response_part(
                    call.name.clone(),
                    call.id.clone(),
                    serde_json::json!({ "result": reason }),
                    thought_sig.clone(),
                ));
                continue;
            }
            let ToolDispatchOutcome {
                result,
                is_error,
                stopped,
                guard_signal,
            } = self
                .dispatch_tool_call_with_executor(
                    &call,
                    current_tools,
                    tool_executor.clone(),
                    iteration_trace_ctx.clone(),
                )
                .await;
            if let Some(signal) = guard_signal {
                response_parts.push(Self::build_function_response_part(
                    call.name.clone(),
                    call.id.clone(),
                    serde_json::json!({
                        "result": signal.message(),
                        "signal": signal.signal()
                    }),
                    thought_sig.clone(),
                ));
                continue;
            }

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
                if let Some(envelope) = Self::parse_tool_envelope(&result) {
                    if let Some(prompt) = envelope.effects.await_user {
                        should_yield_to_user = true;
                        skip_remaining = true;
                        self.output
                            .on_text(&format!("{}\n", Self::format_user_prompt(&prompt)))
                            .await;
                    }
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

        if self.is_autopilot {
            let todos_after = self.count_todos_status();
            if todos_before != todos_after {
                self.output
                    .on_text(&format!(
                        "[System] Autopilot 任务进度已更新: {} 已完成, {} 待办\n",
                        todos_after.0, todos_after.1
                    ))
                    .await;
            }
        }

        (response_parts, should_yield_to_user)
    }

    /// Generate a user-facing status summary of current unfinished work via the LLM.
    /// Falls back to a structural status report if the LLM call fails.
    pub(super) async fn generate_status_summary(&self) -> String {
        // Build a concise text representation of recent history for summarization
        let mut history_text = String::new();
        let max_history_chars = 8_000;

        for turn in self.context.dialogue_history.iter().rev().take(10) {
            let mut turn_desc = format!("[User] {}\n", turn.user_message);
            for msg in &turn.messages {
                for part in &msg.parts {
                    if let Some(fc) = &part.function_call {
                        turn_desc.push_str(&format!(
                            "  → {}({})\n",
                            fc.name,
                            crate::context::AgentContext::truncate_chars(&fc.args.to_string(), 100)
                        ));
                    }
                    if let Some(fr) = &part.function_response {
                        let ok = fr
                            .response
                            .get("ok")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true);
                        let label = if ok { "✓" } else { "✗" };
                        turn_desc.push_str(&format!(
                            "  {} {} -> {}\n",
                            label,
                            fr.name,
                            crate::context::AgentContext::truncate_chars(
                                &fr.response.to_string(),
                                80
                            )
                        ));
                    }
                }
            }
            if history_text.len() + turn_desc.len() > max_history_chars {
                break;
            }
            history_text.push_str(&turn_desc);
            history_text.push('\n');
        }

        // Collect optional supplementary signals
        let task_hint = self
            .task_state_store
            .load()
            .ok()
            .and_then(|s| {
                s.goal
                    .as_ref()
                    .map(|g| format!("\nOriginal task goal: {}", g))
            })
            .unwrap_or_default();

        let (completed, uncompleted) = self.count_todos_status();
        let todos_hint = if completed + uncompleted > 0 {
            format!(
                "\nTODOS progress: {} completed, {} remaining",
                completed, uncompleted
            )
        } else {
            String::new()
        };

        let summary_prompt = format!(
            "You are an AI agent that has paused mid-task. Based on the execution history below, \
             generate a concise status report for the user.\n\n\
             Focus on:\n\
             1. What is the current task objective\n\
             2. What has been accomplished so far (briefly)\n\
             3. What remains unfinished (key focus)\n\
             4. Any errors or blockers encountered\n\
             5. Suggested next steps\n\n\
             Do NOT produce a chronological history recap. Write an actionable status report \
             (max 300 words).{task_hint}{todos_hint}\n\n---\n{history_text}\n---"
        );

        let messages = vec![Message {
            role: "user".to_string(),
            parts: vec![Part {
                text: Some(summary_prompt),
                function_call: None,
                function_response: None,
                thought_signature: None,
                file_data: None,
            }],
        }];

        // Try LLM-based summary with a timeout
        match tokio::time::timeout(
            Duration::from_secs(30),
            self.llm.stream(messages, None, vec![]),
        )
        .await
        {
            Ok(Ok(mut rx)) => {
                let mut summary = String::new();
                while let Some(event) = rx.recv().await {
                    match event {
                        StreamEvent::Text(t) => summary.push_str(&t),
                        StreamEvent::Thought(t) => summary.push_str(&t),
                        StreamEvent::Done | StreamEvent::Error(_) => break,
                        _ => {}
                    }
                }
                let summary = summary.trim().to_string();
                if !summary.is_empty() {
                    return summary;
                }
            }
            Ok(Err(e)) => {
                tracing::warn!("Status summary generation failed: {}", e);
            }
            Err(_) => {
                tracing::warn!("Status summary generation timed out");
            }
        }

        // Fallback: user-facing structural status report
        let mut fallback = String::new();

        // Task goal (from TaskState if available)
        if let Ok(state) = self.task_state_store.load() {
            if let Some(goal) = &state.goal {
                fallback.push_str(&format!("📋 任务目标: {}\n\n", goal));
            }
        }

        // Recent actions summary
        fallback.push_str("📊 最近执行:\n");
        for turn in self.context.dialogue_history.iter().rev().take(3) {
            fallback.push_str(&format!(
                "- {}\n",
                crate::context::AgentContext::truncate_chars(&turn.user_message, 100)
            ));
            let mut tool_count = 0;
            let mut error_count = 0;
            for msg in &turn.messages {
                for part in &msg.parts {
                    if part.function_call.is_some() {
                        tool_count += 1;
                    }
                    if let Some(fr) = &part.function_response {
                        if fr.response.get("ok").and_then(|v| v.as_bool()) == Some(false) {
                            error_count += 1;
                        }
                    }
                }
            }
            if error_count > 0 {
                fallback.push_str(&format!("  ({} 操作, {} 失败)\n", tool_count, error_count));
            } else {
                fallback.push_str(&format!("  ({} 操作, 均成功)\n", tool_count));
            }
        }

        // TODOS progress (if any)
        if completed + uncompleted > 0 {
            fallback.push_str(&format!(
                "\n📝 TODOS: {} 已完成, {} 待完成\n",
                completed, uncompleted
            ));
        }

        fallback
    }

    pub(super) async fn reconcile_after_tool_calls(
        &mut self,
        state_before_tools: &crate::task_state::TaskStateSnapshot,
        iteration_trace_ctx: Option<crate::trace::TraceContext>,
    ) -> crate::task_state::TaskStateSnapshot {
        let state_after_tools = self
            .task_state_store
            .load()
            .unwrap_or_else(|_| crate::task_state::TaskStateSnapshot::empty());
        if state_before_tools != &state_after_tools {
            self.output.on_plan_update(&state_after_tools).await;
            if let Some(ctx) = iteration_trace_ctx.as_ref() {
                self.trace_bus.record_event(
                    ctx,
                    TraceActor::Context,
                    "task_state_changed",
                    TraceStatus::Ok,
                    Some(state_after_tools.status.clone()),
                    serde_json::json!({
                        "goal": state_after_tools.goal,
                        "current_step": state_after_tools.current_step,
                        "plan_steps": state_after_tools.plan_steps,
                    }),
                );
                self.trace_bus.record_event(
                    ctx,
                    TraceActor::Context,
                    "plan_updated",
                    TraceStatus::Ok,
                    Some(state_after_tools.summary()),
                    serde_json::json!({
                        "status": state_after_tools.status,
                    }),
                );
            }
        }

        let current_turn_max_bytes = (self.context.max_history_tokens / 5)
            .saturating_mul(4)
            .clamp(64 * 1024, 400 * 1024);
        let compressed = self.context.compress_current_turn(current_turn_max_bytes);
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
            if let Some(ctx) = iteration_trace_ctx.as_ref() {
                if compressed > 0 {
                    self.trace_bus.record_event(
                        ctx,
                        TraceActor::Context,
                        "context_compacted",
                        TraceStatus::Ok,
                        Some(format!("compressed {} entries", compressed)),
                        serde_json::json!({
                            "compressed_entries": compressed,
                        }),
                    );
                }
                if truncated > 0 {
                    self.trace_bus.record_event(
                        ctx,
                        TraceActor::Context,
                        "tool_result_truncated",
                        TraceStatus::Ok,
                        Some(format!("truncated {} tool results", truncated)),
                        serde_json::json!({
                            "truncated_entries": truncated,
                        }),
                    );
                }
            }
        }

        state_after_tools
    }
}
