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
                                        tool_calls_accumulated.push((tc, sig));
                                    }
                                    Some(StreamEvent::Done) | None => break Ok(()),
                                    Some(StreamEvent::Error(e)) => {
                                        break Err(crate::llm_client::LlmError::ApiError(format!("Stream error: {}", e)));
                                    }
                                }
                            }
                            _ = self.cancel_token.notified() => {
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

                            break current_turn_text;
                        }
                        Err(e) => {
                            tool_calls_accumulated.clear(); // 清理避免重试时带有历史 tool_call
                            if !self.handle_llm_error(&e, llm_attempts).await {
                                self.output.flush().await;
                                return Err(Box::new(e));
                            }
                        }
                    }
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
        serde_json::from_str(result).ok()
    }

    pub(super) fn extract_finish_task_summary_from_result(result: &str) -> Option<String> {
        Self::parse_tool_envelope(result).and_then(|envelope| envelope.finish_task_summary)
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

    pub(super) fn load_current_tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut current_tools = self.tools.clone();
        for skill in crate::skills::load_skills("skills") {
            current_tools.push(Arc::new(skill));
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
        exit
    }

    pub(super) async fn finalize_finished_run(&mut self, summary: String) -> RunExit {
        self.context.end_turn();
        self.telemetry.end_span("agent_step");
        RunExit::Finished(summary)
    }

    pub(super) async fn check_loop_guards(
        &mut self,
        task_state: &mut TaskState,
    ) -> Option<RunExit> {
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

    pub(super) async fn collect_iteration_response(
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

    pub(super) async fn dispatch_tool_call(
        &self,
        call: &crate::context::FunctionCall,
        current_tools: &[Arc<dyn Tool>],
    ) -> ToolDispatchOutcome {
        let tool_opt = current_tools.iter().find(|tool| tool.name() == call.name);

        if let Some(tool) = tool_opt {
            self.output
                .on_tool_start(&call.name, &call.args.to_string())
                .await;

            let ctx = crate::tools::ToolContext {
                session_id: self.session_id.clone(),
                reply_to: self.session_id.clone(),
            };

            let (result, is_error, stopped) = tokio::select! {
                exec_res = tokio::time::timeout(
                    Duration::from_secs(120),
                    tool.execute(call.args.clone(), &ctx)
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

    pub(super) async fn handle_successful_tool_effects(&mut self, result: &str) {
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

    pub(super) async fn execute_tool_round(
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

    pub(super) async fn reconcile_after_tool_calls(
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
}
