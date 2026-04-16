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

    pub(super) fn extract_finish_task_summary_from_result(result: &str) -> Option<String> {
        Self::parse_tool_envelope(result).and_then(|envelope| envelope.effects.finish_task_summary)
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
            self.record_trace_event(
                TraceActor::System,
                "yielded_to_user",
                TraceStatus::Yielded,
                Some("No tool call was emitted".to_string()),
                serde_json::json!({}),
                self.turn_span_id(),
                None,
            );
            self.output.flush().await;
            self.context.end_turn();
            self.telemetry.end_span("agent_step");
            return Some(RunExit::YieldedToUser);
        }

        None
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
                .filter(|name| Self::is_code_mode_nested_tool(name))
                .collect();
            let code_mode_notice =
                crate::code_mode::description::execution_notice(&available_nested_tools);
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

        self.collect_stream_response(
            messages,
            system,
            current_tools.to_vec(),
            iteration_trace_ctx,
        )
        .await
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

    fn is_code_mode_nested_tool(tool_name: &str) -> bool {
        crate::code_mode::executor::is_code_mode_nested_tool(tool_name)
    }

    fn autopilot_denial_for_call(
        &self,
        call_name: &str,
        call_args: &serde_json::Value,
        current_tools: &[Arc<dyn Tool>],
    ) -> Option<String> {
        if !self.is_autopilot {
            return None;
        }

        let tool_has_effects = current_tools
            .iter()
            .find(|t| t.name() == call_name)
            .map(|t| t.has_side_effects())
            .unwrap_or(true);
        if !tool_has_effects {
            return None;
        }

        let todos_path = self.todos_path();
        if todos_path.exists() {
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
        call_args: &serde_json::Value,
        is_error: bool,
    ) -> Option<(String, &'static str)> {
        if !self.is_autopilot {
            return None;
        }

        let mut guard_state = self
            .execution_guard_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard_state
            .record_action_outcome(call_name, call_args, is_error)
            .map(|signal| (signal.message().to_string(), signal.signal()))
    }

    async fn prepare_tool_context(
        &self,
        current_tools: &[Arc<dyn Tool>],
        remaining_steps: usize,
        trace_ctx: Option<&crate::trace::TraceContext>,
        parent_span_id: Option<String>,
    ) -> crate::tools::ToolContext {
        let mut ctx =
            crate::tools::ToolContext::new(self.session_id.clone(), self.reply_to.clone());
        ctx.visible_tools = current_tools.iter().map(|tool| tool.name()).collect();
        ctx.skill_budget.remaining_steps = Some(remaining_steps);
        ctx.skill_budget.remaining_timeout_sec = self.remaining_session_timeout_sec();
        if let Some(trace_ctx) = trace_ctx {
            ctx.trace = Some(crate::tools::protocol::ToolTraceContext {
                trace_id: trace_ctx.trace_id.clone(),
                run_id: trace_ctx.run_id.clone(),
                root_session_id: trace_ctx.root_session_id.clone(),
                task_id: trace_ctx.task_id.clone(),
                turn_id: trace_ctx.turn_id.clone(),
                iteration: trace_ctx.iteration,
                parent_span_id,
            });
        }
        for ext in &self.extensions {
            ctx = ext.enrich_tool_context(ctx).await;
        }
        ctx
    }

    async fn dispatch_exec_tool_call(
        &mut self,
        call: &crate::context::FunctionCall,
        current_tools: &[Arc<dyn Tool>],
        remaining_steps: usize,
        iteration_trace_ctx: Option<crate::trace::TraceContext>,
        parent_span_id: Option<String>,
    ) -> ToolDispatchOutcome {
        let start = Instant::now();
        let iteration = iteration_trace_ctx.as_ref().and_then(|ctx| ctx.iteration);
        let provider = self.llm.provider_name().to_string();
        let model = self.llm.model_name().to_string();

        enum CodeModeInvocation {
            Exec(crate::tools::code_mode::ExecArgs),
            Wait(crate::tools::code_mode::WaitArgs),
        }

        let invocation = match call.name.as_str() {
            "exec" => {
                match serde_json::from_value::<crate::tools::code_mode::ExecArgs>(call.args.clone())
                {
                    Ok(parsed) => CodeModeInvocation::Exec(parsed),
                    Err(err) => {
                        let result = crate::tools::protocol::StructuredToolOutput::new(
                        "exec",
                        false,
                        format!("Invalid exec arguments: {}", err),
                        Some(1),
                        Some(start.elapsed().as_millis()),
                        false,
                    )
                    .with_payload_kind("code_mode_exec")
                    .to_json_string()
                    .unwrap_or_else(|serialize_err| {
                        format!("Tool error: failed to serialize exec error envelope: {serialize_err}")
                    });
                        return ToolDispatchOutcome {
                            result,
                            is_error: true,
                            stopped: false,
                        };
                    }
                }
            }
            "wait" => {
                match serde_json::from_value::<crate::tools::code_mode::WaitArgs>(call.args.clone())
                {
                    Ok(parsed) => CodeModeInvocation::Wait(parsed),
                    Err(err) => {
                        let result = crate::tools::protocol::StructuredToolOutput::new(
                        "wait",
                        false,
                        format!("Invalid wait arguments: {}", err),
                        Some(1),
                        Some(start.elapsed().as_millis()),
                        false,
                    )
                    .with_payload_kind("code_mode_exec")
                    .to_json_string()
                    .unwrap_or_else(|serialize_err| {
                        format!("Tool error: failed to serialize wait error envelope: {serialize_err}")
                    });
                        return ToolDispatchOutcome {
                            result,
                            is_error: true,
                            stopped: false,
                        };
                    }
                }
            }
            _ => {
                return ToolDispatchOutcome {
                    result: format!("Tool `{}` is not a code-mode entry tool.", call.name),
                    is_error: true,
                    stopped: false,
                };
            }
        };

        let (source_length, requested_cell_id, wait_timeout_ms) = match &invocation {
            CodeModeInvocation::Exec(parsed) => (parsed.code.chars().count(), None, None),
            CodeModeInvocation::Wait(parsed) => {
                (0usize, parsed.cell_id.clone(), parsed.wait_timeout_ms)
            }
        };

        let visible_tools: Vec<String> = current_tools
            .iter()
            .map(|tool| tool.name())
            .filter(|name| Self::is_code_mode_nested_tool(name))
            .collect();

        self.record_trace_event(
            TraceActor::Tool,
            "code_mode_exec_started",
            TraceStatus::Ok,
            Some(call.name.clone()),
            serde_json::json!({
                "tool_name": call.name,
                "outer_tool_call_id": call.id.clone(),
                "provider": provider,
                "model": model,
                "source_length": source_length,
                "requested_cell_id": requested_cell_id,
                "wait_timeout_ms": wait_timeout_ms,
                                "visible_nested_tools": visible_tools.len(),
                "args_preview": crate::context::AgentContext::truncate_chars(&call.args.to_string(), 500),
            }),
            parent_span_id.clone(),
            iteration,
        );

        let service = self.code_mode_service.clone();
        let service_for_exec = service.clone();
        let service_for_abort = service.clone();
        let session_id = self.session_id.clone();
        let session_id_for_exec = session_id.clone();
        let session_id_for_abort = session_id.clone();
        let cancel_token = self.cancel_token.clone();
        let reply_to = self.reply_to.clone();
        let session_deadline = self.session_deadline;
        let trace_bus = self.trace_bus.clone();
        let nested_cancel_token = self.cancel_token.clone();
        let todos_path = self.todos_path();
        let is_autopilot = self.is_autopilot;
        let exec_result = tokio::select! {
            result = tokio::time::timeout(
                Duration::from_secs(90),
                async {
                    match invocation {
                        CodeModeInvocation::Exec(parsed) => {
                            let cell_span = iteration_trace_ctx.as_ref().map(|ctx| {
                                trace_bus.start_span(
                                    ctx,
                                    crate::trace::TraceActor::Tool,
                                    "code_mode_cell_background",
                                    serde_json::json!({
                                        "session_id": session_id,
                                        "outer_tool_call_id": call.id.clone(),
                                    }),
                                )
                            });
                            let cell_span_id = cell_span.as_ref().map(|s| s.span_id().to_string());

                            let nested_executor = Arc::new(tokio::sync::Mutex::new(
                                crate::code_mode::executor::CodeModeNestedToolExecutor::new(
                                    crate::code_mode::executor::CodeModeNestedToolExecutorConfig {
                                        current_tools: current_tools.to_vec(),
                                        extensions: self.extensions.clone(),
                                        session_id: session_id.clone(),
                                        reply_to,
                                        remaining_steps,
                                        session_deadline,
                                        iteration_trace_ctx: iteration_trace_ctx.clone(),
                                        parent_span_id: cell_span_id,
                                        outer_tool_call_id: call.id.clone(),
                                        trace_bus,
                                        provider: provider.clone(),
                                        model: model.clone(),
                                        cancel_token: nested_cancel_token,
                                        is_autopilot,
                                        todos_path,
                                        execution_guard_state: self.execution_guard_state.clone(),
                                    },
                                ),
                            ));
                            let invoke_tool = move |tool_name: String, args_json: String| {
                                let nested_executor = nested_executor.clone();
                                async move {
                                    let mut executor = nested_executor.lock().await;
                                    let raw = executor.execute_json(tool_name, args_json).await?;
                                    Ok(crate::code_mode::runtime::value::normalize_tool_result_for_js(&raw))
                                }
                            };

                            service_for_exec.execute(
                                &session_id_for_exec,
                                &parsed.code,
                                visible_tools,
                                invoke_tool,
                                cell_span,
                            ).await
                        }
                        CodeModeInvocation::Wait(parsed) => {
                            service_for_exec.wait_with_request(
                                &session_id_for_exec,
                                parsed.cell_id.as_deref(),
                                parsed.wait_timeout_ms,
                            ).await
                        }
                    }
                }
            ) => {
                match result {
                    Ok(Ok(value)) => Ok(value),
                    Ok(Err(err)) => Err(err),
                    Err(_) => {
                        service_for_abort
                            .abort_active_cell(&session_id_for_abort, "Code mode execution timed out.")
                            .await;
                        Err(crate::tools::ToolError::Timeout)
                    }
                }
            }
            _ = cancel_token.notified() => {
                service
                    .abort_active_cell(&session_id, "Code mode execution interrupted by user.")
                    .await;
                Err(crate::tools::ToolError::ExecutionFailed(
                    "Code mode execution interrupted by user.".to_string(),
                ))
            }
        };

        match exec_result {
            Ok(summary) => {
                let (event_name, event_status) = if summary.flushed {
                    ("code_mode_exec_flushed", TraceStatus::Ok)
                } else if summary.failure.is_some() {
                    ("code_mode_exec_failed", TraceStatus::Error)
                } else {
                    ("code_mode_exec_finished", TraceStatus::Ok)
                };
                let cell_id = summary.cell_id.clone();
                let flushed = summary.flushed;
                let nested_tool_calls = summary.nested_tool_calls;
                let truncated = summary.truncated;
                let termination_reason = if summary.flushed {
                    if summary.waiting_on_timer_ms.is_some() {
                        "waiting_on_timer"
                    } else {
                        "flush"
                    }
                } else if summary.failure.is_some() {
                    "failed"
                } else {
                    "completed"
                };
                self.record_trace_event(
                    TraceActor::Tool,
                    event_name,
                    event_status,
                    Some(cell_id.clone()),
                    serde_json::json!({
                        "tool_name": call.name.clone(),
                        "outer_tool_call_id": call.id.clone(),
                        "provider": provider,
                        "model": model,
                        "cell_id": cell_id,
                        "source_length": source_length,
                        "requested_cell_id": requested_cell_id,
                        "flushed": flushed,
                        "flush_value": summary.flush_value,
                        "nested_tool_calls": nested_tool_calls,
                        "output_size_chars": summary.output_text.chars().count(),
                        "termination_reason": termination_reason,
                        "truncated": truncated,
                    }),
                    parent_span_id,
                    iteration,
                );

                let mut tool_output = crate::tools::protocol::StructuredToolOutput::new(
                    call.name.clone(),
                    true,
                    summary.render_output(),
                    Some(0),
                    Some(start.elapsed().as_millis()),
                    summary.truncated,
                )
                .with_payload_kind("code_mode_exec");

                if summary.flushed {
                    let question = if let Some(delay_ms) = summary.waiting_on_timer_ms {
                        format!(
                            "Code mode is waiting on a timer. Call `wait` again in about {} ms.",
                            delay_ms
                        )
                    } else {
                        let flush_value_str = summary
                            .flush_value
                            .as_ref()
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "none".to_string());
                        format!(
                            "Code mode flushed. Value: {}. Call `wait` to continue.",
                            flush_value_str
                        )
                    };
                    tool_output =
                        tool_output.with_await_user(crate::tools::protocol::UserPromptRequest {
                            question,
                            context_key: format!("code_mode_flush_{}", summary.cell_id),
                            options: vec!["continue".to_string(), "cancel".to_string()],
                            recommendation: Some("continue".to_string()),
                        });
                }

                let output_text = summary.render_output();
                if !output_text.is_empty() {
                    self.output.on_text(&output_text).await;
                    self.output.on_text("\n").await;
                }

                let result = tool_output
                    .to_json_string()
                    .unwrap_or_else(|err| format!("Tool error: {}", err));

                ToolDispatchOutcome {
                    result,
                    is_error: false,
                    stopped: false,
                }
            }
            Err(err) => {
                let is_stopped = matches!(err, crate::tools::ToolError::Timeout)
                    || err.to_string().contains("interrupted by user");
                let (event_name, event_status) = if matches!(err, crate::tools::ToolError::Timeout)
                {
                    ("code_mode_exec_terminated", TraceStatus::TimedOut)
                } else if err.to_string().contains("interrupted by user") {
                    ("code_mode_exec_terminated", TraceStatus::Cancelled)
                } else {
                    ("code_mode_exec_finished", TraceStatus::Error)
                };
                self.record_trace_event(
                    TraceActor::Tool,
                    event_name,
                    event_status,
                    Some(err.to_string()),
                    serde_json::json!({
                        "tool_name": call.name.clone(),
                        "outer_tool_call_id": call.id.clone(),
                        "provider": provider,
                        "model": model,
                        "source_length": source_length,
                        "requested_cell_id": requested_cell_id,
                        "termination_reason": if matches!(err, crate::tools::ToolError::Timeout) {
                            "timeout"
                        } else if err.to_string().contains("interrupted by user") {
                            "cancelled"
                        } else {
                            "runtime_error"
                        },
                        "error": err.to_string(),
                    }),
                    parent_span_id,
                    iteration,
                );

                let result = crate::tools::protocol::StructuredToolOutput::new(
                    call.name.clone(),
                    false,
                    format!("{} runtime failed: {}", call.name, err),
                    Some(1),
                    Some(start.elapsed().as_millis()),
                    false,
                )
                .with_payload_kind("code_mode_exec")
                .to_json_string()
                .unwrap_or_else(|serialize_err| format!("Tool error: {}", serialize_err));

                ToolDispatchOutcome {
                    result,
                    is_error: true,
                    stopped: is_stopped,
                }
            }
        }
    }

    pub(super) async fn dispatch_tool_call(
        &mut self,
        call: &crate::context::FunctionCall,
        current_tools: &[Arc<dyn Tool>],
        remaining_steps: usize,
        iteration_trace_ctx: Option<crate::trace::TraceContext>,
    ) -> ToolDispatchOutcome {
        let tool_opt = current_tools.iter().find(|tool| tool.name() == call.name);

        if let Some(tool) = tool_opt {
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
                        "remaining_steps": remaining_steps,
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
                };
            }

            if matches!(call.name.as_str(), "exec" | "wait") {
                let exec_outcome = self
                    .dispatch_exec_tool_call(
                        call,
                        current_tools,
                        remaining_steps,
                        iteration_trace_ctx.clone(),
                        tool_span.as_ref().map(|span| span.span_id().to_string()),
                    )
                    .await;
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

            let ctx = self
                .prepare_tool_context(
                    current_tools,
                    remaining_steps,
                    iteration_trace_ctx.as_ref(),
                    tool_span.as_ref().map(|span| span.span_id().to_string()),
                )
                .await;

            let (result, is_error, stopped, trace_status, end_name) = tokio::select! {
                exec_res = tokio::time::timeout(
                    Duration::from_secs(120),
                    tool.execute(call.args.clone(), &ctx)
                ) => {
                    match exec_res {
                        Ok(Ok(res)) => {
                            tracing::info!("Tool '{}' executed successfully", call.name);
                            (res, false, false, TraceStatus::Ok, "tool_finished")
                        },
                        Ok(Err(e)) => {
                            tracing::warn!("Tool '{}' returned an error: {}", call.name, e);
                            (format!("Tool error: {}", e), true, false, TraceStatus::Error, "tool_failed")
                        },
                        Err(e) => {
                            tracing::error!("Tool '{}' timed out: {}", call.name, e);
                            (format!("Timeout executing {}: {}", call.name, e), true, false, TraceStatus::TimedOut, "tool_timed_out")
                        },
                    }
                }
                _ = self.cancel_token.notified() => {
                    tracing::warn!("Tool execution '{}' interrupted by user", call.name);
                    ("Tool execution interrupted by user.".to_string(), true, true, TraceStatus::Cancelled, "tool_cancelled")
                }
            };
            if let Some(span) = tool_span.take() {
                span.finish(
                    end_name,
                    trace_status,
                    Some(crate::context::AgentContext::truncate_chars(&result, 240)),
                    serde_json::json!({
                        "tool_name": call.name,
                        "result_preview": crate::context::AgentContext::truncate_chars(&result, 500),
                        "result_size_chars": result.len(),
                    }),
                );
            }

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
        state: &mut crate::task_state::TaskStateSnapshot,
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

            if let Some(reason) =
                self.autopilot_denial_for_call(&call.name, &call.args, current_tools)
            {
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
            } = self
                .dispatch_tool_call(
                    &call,
                    current_tools,
                    remaining_steps,
                    iteration_trace_ctx.clone(),
                )
                .await;
            if let Some((message, signal)) =
                self.record_autopilot_action_outcome(&call.name, &call.args, is_error)
            {
                response_parts.push(Self::build_function_response_part(
                    call.name.clone(),
                    call.id.clone(),
                    serde_json::json!({
                        "result": message,
                        "signal": signal
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

                if let Some(summary) = Self::extract_finish_task_summary_from_result(&result) {
                    if self.is_autopilot && self.has_uncompleted_todos() {
                        response_parts.push(Self::build_function_response_part(
                            call.name.clone(),
                            call.id.clone(),
                            serde_json::json!({ "result": "[System Error] Action Denied. Autopilot 模式下必须完成 TODOS.md 中的所有任务才能结束。" }),
                            thought_sig.clone(),
                        ));
                        continue;
                    }
                    state.status = "finished".to_string();
                    state.finish_summary = Some(summary.clone());
                    // Mark all steps as completed when finishing
                    for step in &mut state.plan_steps {
                        step.status = "completed".to_string();
                    }
                    state.current_step = None;
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
