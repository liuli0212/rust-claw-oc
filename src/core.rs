use crate::artifact_store::ArtifactStore;
use crate::context::{AgentContext, ContextDiff, FunctionResponse, Message, Part};
use crate::event_log::{
    read_event_log, AgentEvent, ArtifactCreatedPayload, EventLogWriter, EventRecord,
    TaskFailedPayload, TaskFinishedPayload, TaskPlanSyncedPayload, TaskStartedPayload,
    TaskStoppedPayload, TaskYieldedPayload, ToolExecutionFinishedPayload,
    ToolExecutionStartedPayload,
};
use crate::evidence::{file_evidence, generic_evidence, Evidence};
use crate::llm_client::{LlmClient, StreamEvent};
use crate::rag::RagSearchHit;
use crate::schema::{new_event_id, new_evidence_id, new_task_id, task_plan_path};
use crate::task_state::{replay_task_state, write_task_state};
use crate::telemetry;
use crate::tools::TaskPlanState;
use crate::tools::Tool;
use async_trait::async_trait;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

#[async_trait]
pub trait AgentOutput: Send + Sync {
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
}

#[derive(Debug, Clone, PartialEq)]
pub enum RunExit {
    Finished(String),
    StoppedByUser,
    AgentTurnLimitReached,
    #[allow(dead_code)]
    ContextLimitReached,
    YieldedToUser,
    #[allow(dead_code)]
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

struct ArtifactizedToolResult {
    response: serde_json::Value,
    artifact_id: String,
    artifact_path: String,
    is_truncated: bool,
}

pub struct AgentLoop {
    session_id: String,
    llm: Arc<dyn LlmClient>,
    tools: Vec<Arc<dyn Tool>>,
    context: AgentContext,
    output: Arc<dyn AgentOutput>,
    event_log: Option<Arc<EventLogWriter>>,
    task_state_path: PathBuf,
    artifact_store: Option<Arc<ArtifactStore>>,
    pub cancel_token: Arc<Notify>,
    pub cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl AgentLoop {
    const MAX_LLM_RECOVERY_ATTEMPTS: usize = 3;
    const MAX_CONSECUTIVE_EMPTY_RESPONSES: usize = 3;
    const MAX_ITERATIONS: usize = 25;
    const INITIAL_ENERGY: usize = 100;
    const ARTIFACTIZE_RESULT_CHARS: usize = 4_000;

    pub fn new(
        session_id: String,
        llm: Arc<dyn LlmClient>,
        tools: Vec<Arc<dyn Tool>>,
        context: AgentContext,
        output: Arc<dyn AgentOutput>,
        event_log: Option<Arc<EventLogWriter>>,
        task_state_path: PathBuf,
        artifact_store: Option<Arc<ArtifactStore>>,
    ) -> Self {
        Self {
            session_id,
            llm,
            tools,
            context,
            output,
            event_log,
            task_state_path,
            artifact_store,
            cancel_token: Arc::new(Notify::new()),
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn update_llm(&mut self, new_llm: Arc<dyn LlmClient>) {
        self.llm = new_llm;
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
        self.context.inspect_context(section, arg)
    }

    pub fn build_llm_payload(
        &self,
    ) -> (Vec<Message>, Option<Message>, crate::context::PromptReport) {
        self.context.build_llm_payload()
    }

    pub async fn force_compact(
        &mut self,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        self.maybe_compact_history(true).await?;
        Ok("Compaction triggered.".to_string())
    }

    async fn maybe_compact_history(
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
            telemetry::record_compaction_decision(num_to_compact, target_tokens, &reason);
            self.output.on_text(&format!("[System] {}\n", reason)).await;
        }

        Ok(())
    }

    fn is_transient_llm_error(err: &crate::llm_client::LlmError) -> bool {
        let msg = format!("{}", err).to_lowercase();
        msg.contains("timeout")
            || msg.contains("500")
            || msg.contains("502")
            || msg.contains("503")
            || msg.contains("rate limit")
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

    async fn emit_event(&self, event: AgentEvent) {
        let Some(writer) = &self.event_log else {
            return;
        };

        let record = EventRecord::from_agent_event(
            new_event_id(),
            self.session_id.clone(),
            unix_now(),
            event,
        );

        if let Err(err) = writer.append(&record).await {
            tracing::warn!("Failed to append event log record: {}", err);
        }
    }

    async fn materialize_task_state(&self, task_id: &str) {
        let Some(writer) = &self.event_log else {
            return;
        };

        let events = match read_event_log(writer.path()).await {
            Ok(events) => events,
            Err(err) => {
                tracing::warn!(
                    "Failed to read event log for task state materialization: {}",
                    err
                );
                return;
            }
        };

        let snapshot = replay_task_state(task_id, &events);
        if let Err(err) = write_task_state(&self.task_state_path, &snapshot).await {
            tracing::warn!("Failed to write task state snapshot: {}", err);
        }
    }

    async fn emit_event_and_refresh(&self, task_id: &str, event: AgentEvent) {
        self.emit_event(event).await;
        self.materialize_task_state(task_id).await;
    }

    fn maybe_artifactize_tool_result(
        &self,
        task_id: &str,
        tool_name: &str,
        result: &str,
        is_truncated: bool,
    ) -> Option<ArtifactizedToolResult> {
        let store = self.artifact_store.as_ref()?;
        if result.chars().count() <= Self::ARTIFACTIZE_RESULT_CHARS {
            return None;
        }

        let summary: String = result.chars().take(160).collect();
        let metadata = store
            .write_tool_artifact(
                &self.session_id,
                task_id,
                tool_name,
                result,
                &summary,
                is_truncated,
            )
            .ok()?;

        telemetry::record_artifact_created(&metadata);

        Some(ArtifactizedToolResult {
            response: serde_json::json!({
                "result": format!("Artifactized tool output. Summary: {}", summary),
                "artifact_id": metadata.artifact_id,
                "artifact_path": metadata.path,
                "is_truncated": metadata.is_truncated,
            }),
            artifact_id: metadata.artifact_id,
            artifact_path: metadata.path,
            is_truncated: metadata.is_truncated,
        })
    }

    pub async fn step(
        &mut self,
        goal: String,
    ) -> Result<RunExit, Box<dyn std::error::Error + Send + Sync>> {
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
        let task_id = new_task_id();
        let turn_span = telemetry::agent_turn_span(&self.session_id, &task_id);
        let _turn_guard = turn_span.enter();

        self.context.start_turn(goal);
        let turn_id = self
            .context
            .current_turn_id()
            .unwrap_or_else(|| "unknown".to_string());
        self.emit_event_and_refresh(
            &task_id,
            AgentEvent::TaskStarted(TaskStartedPayload {
                task_id: task_id.clone(),
                turn_id,
                goal: self.context.current_user_message().unwrap_or_default(),
            }),
        )
        .await;

        loop {
            // Check persistent cancel flag at top of each iteration
            if self.is_cancelled() {
                self.emit_event_and_refresh(
                    &task_id,
                    AgentEvent::TaskStopped(TaskStoppedPayload {
                        task_id: task_id.clone(),
                        reason: "cancelled".to_string(),
                    }),
                )
                .await;
                self.context.end_turn();
                return Ok(RunExit::StoppedByUser);
            }
            if task_state.iterations >= Self::MAX_ITERATIONS {
                tracing::warn!(
                    "Agent loop reached MAX_ITERATIONS ({}). Exiting to prevent runaway loops.",
                    Self::MAX_ITERATIONS
                );
                self.emit_event_and_refresh(
                    &task_id,
                    AgentEvent::TaskStopped(TaskStoppedPayload {
                        task_id: task_id.clone(),
                        reason: "turn_limit_reached".to_string(),
                    }),
                )
                .await;
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
                self.emit_event_and_refresh(
                    &task_id,
                    AgentEvent::TaskFailed(TaskFailedPayload {
                        task_id: task_id.clone(),
                        reason: "energy_depleted".to_string(),
                    }),
                )
                .await;
                self.context.end_turn();
                return Ok(RunExit::CriticallyFailed("Energy depleted".to_string()));
            }

            if !compaction_checked {
                let _ = self.maybe_compact_history(false).await;
                compaction_checked = true;
            }

            let (messages, system, _) = self.context.build_llm_payload();

            // Dynamically load skills on every turn so we don't need to restart
            let mut current_tools = self.tools.clone();
            for skill in crate::skills::load_skills("skills") {
                current_tools.push(Arc::new(skill));
            }

            let mut llm_attempts = 0;
            let mut tool_calls_accumulated: Vec<(crate::context::FunctionCall, Option<String>)> =
                Vec::new();

            let full_text = loop {
                llm_attempts += 1;

                let stream_res = tokio::select! {
                    res = self.llm.stream(messages.clone(), system.clone(), current_tools.clone()) => res,
                    _ = self.cancel_token.notified() => {
                        self.emit_event_and_refresh(
                            &task_id,
                            AgentEvent::TaskStopped(TaskStoppedPayload {
                                task_id: task_id.clone(),
                                reason: "cancelled".to_string(),
                            }),
                        ).await;
                        self.context.end_turn();
                        return Ok(RunExit::StoppedByUser);
                    }
                };

                match stream_res {
                    Ok(mut rx) => {
                        let mut current_turn_text = String::new();
                        let mut in_think_block = false;

                        let stream_loop_res = loop {
                            tokio::select! {
                                event = rx.recv() => {
                                    match event {
                                        Some(StreamEvent::Text(t)) => {
                                            current_turn_text.push_str(&t);
                                            let mut remaining = t.as_str();
                                            while !remaining.is_empty() {
                                                if in_think_block {
                                                    if let Some(end_idx) = remaining.find("</think>") {
                                                        let before = &remaining[..end_idx];
                                                        if !before.is_empty() {
                                                            self.output.on_thinking(before).await;
                                                        }
                                                        in_think_block = false;
                                                        remaining = &remaining[end_idx + 8..];
                                                    } else {
                                                        self.output.on_thinking(remaining).await;
                                                        break;
                                                    }
                                                } else {
                                                    if let Some(start_idx) = remaining.find("<think>") {
                                                        let before = &remaining[..start_idx];
                                                        if !before.is_empty() {
                                                            self.output.on_text(before).await;
                                                        }
                                                        in_think_block = true;
                                                        remaining = &remaining[start_idx + 7..];
                                                    } else {
                                                        self.output.on_text(remaining).await;
                                                        break;
                                                    }
                                                }
                                            }
                                        }
                                        Some(StreamEvent::Thought(t)) => {
                                            self.output.on_thinking(&t).await;
                                            current_turn_text.push_str(&format!("<think>{}</think>", t));
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
                            self.emit_event_and_refresh(
                                &task_id,
                                AgentEvent::TaskStopped(TaskStoppedPayload {
                                    task_id: task_id.clone(),
                                    reason: exit.label().to_string(),
                                }),
                            )
                            .await;
                            self.context.end_turn();
                            return Ok(exit);
                        }

                        break current_turn_text;
                    }
                    Err(e) => {
                        if !self.handle_llm_error(&e, llm_attempts).await {
                            return Err(Box::new(e));
                        }
                    }
                }
            };

            if full_text.trim().is_empty() && tool_calls_accumulated.is_empty() {
                consecutive_empty_responses += 1;
                if consecutive_empty_responses >= Self::MAX_CONSECUTIVE_EMPTY_RESPONSES {
                    self.emit_event_and_refresh(
                        &task_id,
                        AgentEvent::TaskFailed(TaskFailedPayload {
                            task_id: task_id.clone(),
                            reason: "empty_responses".to_string(),
                        }),
                    )
                    .await;
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

            if tool_calls_accumulated.is_empty() {
                self.emit_event_and_refresh(
                    &task_id,
                    AgentEvent::TaskYielded(TaskYieldedPayload {
                        task_id: task_id.clone(),
                    }),
                )
                .await;
                self.output.flush().await;
                self.context.end_turn();
                return Ok(RunExit::YieldedToUser);
            }

            let mut executed_signatures = HashSet::new();
            let mut stop_loop = false;

            for (mut call, _thought_sig) in tool_calls_accumulated {
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

                self.emit_event_and_refresh(
                    &task_id,
                    AgentEvent::ToolExecutionStarted(ToolExecutionStartedPayload {
                        task_id: task_id.clone(),
                        turn_id: self.context.current_turn_id(),
                        tool_name: call.name.clone(),
                        tool_call_id: call.id.clone(),
                        args: call.args.clone(),
                    }),
                )
                .await;

                if call.name == "finish_task" {
                    if let Ok(current_dir) = std::env::current_dir() {
                        let _ = std::fs::remove_file(task_plan_path(&current_dir));
                    }
                    let mut summary = call.args.to_string();
                    if let Some(obj) = call.args.as_object() {
                        if let Some(s) = obj.get("summary").and_then(|v| v.as_str()) {
                            summary = s.to_string();
                        }
                    }
                    self.output.flush().await;
                    self.output.on_text(&format!("\n{}\n", summary)).await;
                    self.output.flush().await;
                    self.emit_event_and_refresh(
                        &task_id,
                        AgentEvent::TaskFinished(TaskFinishedPayload {
                            task_id: task_id.clone(),
                            summary: summary.clone(),
                        }),
                    )
                    .await;
                    self.context.end_turn();
                    return Ok(RunExit::Finished(summary));
                }

                let tool_opt = self.tools.iter().find(|t| t.name() == call.name);
                let (result, is_error, stopped) = if let Some(tool) = tool_opt {
                    let tool_span =
                        telemetry::tool_execution_span(&self.session_id, &task_id, &call.name);
                    let _tool_guard = tool_span.enter();
                    self.output.flush().await;
                    self.output
                        .on_tool_start(&call.name, &call.args.to_string())
                        .await;

                    tokio::select! {
                        exec_res = tool.execute(call.args.clone()) => {
                            match exec_res {
                                Ok(res) => (res, false, false),
                                Err(e) => (format!("Error executing {}: {}", call.name, e), true, false),
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
                    self.emit_event_and_refresh(
                        &task_id,
                        AgentEvent::ToolExecutionFinished(ToolExecutionFinishedPayload {
                            task_id: task_id.clone(),
                            tool_name: call.name.clone(),
                            tool_call_id: call.id.clone(),
                            status: "cancelled".to_string(),
                            result_preview: result.chars().take(200).collect::<String>(),
                        }),
                    )
                    .await;
                    self.output.on_error(&result).await;
                    stop_loop = true;
                    break;
                }

                if is_error {
                    self.emit_event_and_refresh(
                        &task_id,
                        AgentEvent::ToolExecutionFinished(ToolExecutionFinishedPayload {
                            task_id: task_id.clone(),
                            tool_name: call.name.clone(),
                            tool_call_id: call.id.clone(),
                            status: "error".to_string(),
                            result_preview: result.chars().take(200).collect::<String>(),
                        }),
                    )
                    .await;
                    self.output.on_error(&result).await;
                } else {
                    self.emit_event_and_refresh(
                        &task_id,
                        AgentEvent::ToolExecutionFinished(ToolExecutionFinishedPayload {
                            task_id: task_id.clone(),
                            tool_name: call.name.clone(),
                            tool_call_id: call.id.clone(),
                            status: "ok".to_string(),
                            result_preview: result.chars().take(200).collect::<String>(),
                        }),
                    )
                    .await;
                    self.output.on_tool_end(&result).await;
                    if call.name == "task_plan" {
                        if let Some(plan_state) = parse_task_plan_state(&result) {
                            self.emit_event_and_refresh(
                                &task_id,
                                AgentEvent::TaskPlanSynced(TaskPlanSyncedPayload {
                                    task_id: task_id.clone(),
                                    plan_state: serde_json::to_value(plan_state)
                                        .unwrap_or(serde_json::Value::Null),
                                }),
                            )
                            .await;
                        }
                    }
                    if call.name == "search_knowledge_base" {
                        if let Some(evidence) = parse_retrieved_evidence(&result) {
                            self.context.set_retrieved_evidence(evidence);
                        }
                    }
                    if call.name == "send_file" {
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&result) {
                            if let Some(path) = val.get("path").and_then(|v| v.as_str()) {
                                self.output.on_file(path).await;
                            }
                        }
                    }
                }

                let artifactized =
                    self.maybe_artifactize_tool_result(&task_id, &call.name, &result, false);

                self.context.add_message_to_current_turn(Message {
                    role: "function".to_string(),
                    parts: vec![Part {
                        text: None,
                        function_call: None,
                        function_response: Some(FunctionResponse {
                            name: call.name.clone(),
                            response: artifactized
                                .as_ref()
                                .map(|item| item.response.clone())
                                .unwrap_or_else(|| serde_json::json!({ "result": result })),
                            tool_call_id: call.id.clone(),
                        }),
                        thought_signature: None,
                    }],
                });

                if let Some(artifact) = artifactized {
                    self.emit_event_and_refresh(
                        &task_id,
                        AgentEvent::ArtifactCreated(ArtifactCreatedPayload {
                            task_id: task_id.clone(),
                            tool_name: call.name.clone(),
                            artifact_id: artifact.artifact_id,
                            artifact_path: artifact.artifact_path,
                            is_truncated: artifact.is_truncated,
                        }),
                    )
                    .await;
                }
            }

            if stop_loop {
                self.emit_event_and_refresh(
                    &task_id,
                    AgentEvent::TaskStopped(TaskStoppedPayload {
                        task_id: task_id.clone(),
                        reason: "stopped_by_user".to_string(),
                    }),
                )
                .await;
                self.context.end_turn();
                return Ok(RunExit::StoppedByUser);
            }
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn parse_task_plan_state(result: &str) -> Option<TaskPlanState> {
    let envelope = serde_json::from_str::<serde_json::Value>(result).ok()?;
    let output = envelope.get("output")?.as_str()?;
    serde_json::from_str::<TaskPlanState>(output).ok()
}

fn parse_retrieved_evidence(result: &str) -> Option<Vec<Evidence>> {
    let envelope = serde_json::from_str::<serde_json::Value>(result).ok()?;
    let data = envelope.get("data")?.as_array()?;
    let retrieved_at = unix_now();
    let mut evidence = Vec::new();

    for hit in data {
        let parsed = serde_json::from_value::<RagSearchHit>(hit.clone()).ok()?;
        let summary = format!(
            "Retrieved from {} (relevance {:.2})",
            parsed.source, parsed.relevance
        );
        let item = if Path::new(&parsed.source).is_file() {
            file_evidence(
                new_evidence_id(),
                Path::new(&parsed.source),
                retrieved_at,
                summary,
                parsed.content,
            )
            .ok()?
        } else {
            generic_evidence(
                new_evidence_id(),
                "knowledge_base".to_string(),
                parsed.source,
                retrieved_at,
                summary,
                parsed.content,
            )
        };
        evidence.push(item);
    }

    Some(evidence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::FunctionCall;
    use crate::event_log::EventLogWriter;
    use crate::llm_client::LlmError;
    use crate::tools::{Tool, ToolError};
    use serde_json::Value;
    use tempfile::tempdir;
    use tokio::sync::{mpsc, Mutex};

    struct TestOutput;

    #[async_trait]
    impl AgentOutput for TestOutput {
        async fn on_text(&self, _text: &str) {}
        async fn on_tool_start(&self, _name: &str, _args: &str) {}
        async fn on_tool_end(&self, _result: &str) {}
        async fn on_error(&self, _error: &str) {}
    }

    struct MockTool;

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> String {
            "mock_tool".to_string()
        }

        fn description(&self) -> String {
            "mock tool".to_string()
        }

        fn parameters_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(&self, _args: Value) -> Result<String, ToolError> {
            Ok("mock_result".to_string())
        }
    }

    struct MockTaskPlanTool;

    #[async_trait]
    impl Tool for MockTaskPlanTool {
        fn name(&self) -> String {
            "task_plan".to_string()
        }

        fn description(&self) -> String {
            "mock task plan tool".to_string()
        }

        fn parameters_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(&self, _args: Value) -> Result<String, ToolError> {
            Ok(serde_json::json!({
                "ok": true,
                "tool_name": "task_plan",
                "output": serde_json::json!({
                    "items": [
                        {"step": "Inspect file", "status": "in_progress", "note": null},
                        {"step": "Apply patch", "status": "pending", "note": null}
                    ]
                }).to_string()
            })
            .to_string())
        }
    }

    struct MockLargeTool;

    #[async_trait]
    impl Tool for MockLargeTool {
        fn name(&self) -> String {
            "mock_large_tool".to_string()
        }

        fn description(&self) -> String {
            "mock large output tool".to_string()
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> Result<String, crate::tools::ToolError> {
            Ok("x".repeat(5_000))
        }
    }

    struct MockLlm {
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl LlmClient for MockLlm {
        fn model_name(&self) -> &str {
            "mock-model"
        }

        fn provider_name(&self) -> &str {
            "mock-provider"
        }

        fn context_window_size(&self) -> usize {
            32_000
        }

        async fn generate_text(
            &self,
            _messages: Vec<Message>,
            _system_instruction: Option<Message>,
        ) -> Result<String, LlmError> {
            Ok(String::new())
        }

        async fn stream(
            &self,
            _messages: Vec<Message>,
            _system_instruction: Option<Message>,
            _tools: Vec<Arc<dyn Tool>>,
        ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
            let mut calls = self.calls.lock().await;
            let call_index = *calls;
            *calls += 1;

            let (tx, rx) = mpsc::channel(8);
            tokio::spawn(async move {
                if call_index == 0 {
                    let _ = tx
                        .send(StreamEvent::ToolCall(
                            FunctionCall {
                                name: "mock_tool".to_string(),
                                args: serde_json::json!({"input": "value"}),
                                id: Some("call_1".to_string()),
                            },
                            None,
                        ))
                        .await;
                } else {
                    let _ = tx.send(StreamEvent::Text("done".to_string())).await;
                }
                let _ = tx.send(StreamEvent::Done).await;
            });

            Ok(rx)
        }
    }

    struct MockTaskPlanLlm {
        calls: Mutex<usize>,
    }

    struct MockLargeToolLlm {
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl LlmClient for MockLargeToolLlm {
        fn model_name(&self) -> &str {
            "mock-model"
        }
        fn provider_name(&self) -> &str {
            "mock-provider"
        }
        fn context_window_size(&self) -> usize {
            32_000
        }
        async fn generate_text(
            &self,
            _messages: Vec<Message>,
            _system_instruction: Option<Message>,
        ) -> Result<String, LlmError> {
            Ok(String::new())
        }
        async fn stream(
            &self,
            _messages: Vec<Message>,
            _system_instruction: Option<Message>,
            _tools: Vec<Arc<dyn Tool>>,
        ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
            let mut calls = self.calls.lock().await;
            let call_index = *calls;
            *calls += 1;
            let (tx, rx) = mpsc::channel(8);
            tokio::spawn(async move {
                if call_index == 0 {
                    let _ = tx
                        .send(StreamEvent::ToolCall(
                            FunctionCall {
                                name: "mock_large_tool".to_string(),
                                args: serde_json::json!({}),
                                id: Some("call_large_1".to_string()),
                            },
                            None,
                        ))
                        .await;
                } else {
                    let _ = tx.send(StreamEvent::Text("done".to_string())).await;
                }
                let _ = tx.send(StreamEvent::Done).await;
            });
            Ok(rx)
        }
    }

    #[async_trait]
    impl LlmClient for MockTaskPlanLlm {
        fn model_name(&self) -> &str {
            "mock-model"
        }

        fn provider_name(&self) -> &str {
            "mock-provider"
        }

        fn context_window_size(&self) -> usize {
            32_000
        }

        async fn generate_text(
            &self,
            _messages: Vec<Message>,
            _system_instruction: Option<Message>,
        ) -> Result<String, LlmError> {
            Ok(String::new())
        }

        async fn stream(
            &self,
            _messages: Vec<Message>,
            _system_instruction: Option<Message>,
            _tools: Vec<Arc<dyn Tool>>,
        ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
            let mut calls = self.calls.lock().await;
            let call_index = *calls;
            *calls += 1;

            let (tx, rx) = mpsc::channel(8);
            tokio::spawn(async move {
                if call_index == 0 {
                    let _ = tx
                        .send(StreamEvent::ToolCall(
                            FunctionCall {
                                name: "task_plan".to_string(),
                                args: serde_json::json!({"action": "add", "step": "Inspect file"}),
                                id: Some("call_plan_1".to_string()),
                            },
                            None,
                        ))
                        .await;
                } else {
                    let _ = tx.send(StreamEvent::Text("done".to_string())).await;
                }
                let _ = tx.send(StreamEvent::Done).await;
            });

            Ok(rx)
        }
    }

    #[tokio::test]
    async fn test_regression_event_log_records_task_and_tool_events() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let writer = Arc::new(EventLogWriter::new(log_path.clone()).await.unwrap());
        let llm: Arc<dyn LlmClient> = Arc::new(MockLlm {
            calls: Mutex::new(0),
        });
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(MockTool)];
        let context = AgentContext::new();
        let output: Arc<dyn AgentOutput> = Arc::new(TestOutput);

        let mut loop_ = AgentLoop::new(
            "cli".to_string(),
            llm,
            tools,
            context,
            output,
            Some(writer),
            dir.path().join("task_state.json"),
            None,
        );

        let exit = loop_.step("test goal".to_string()).await.unwrap();
        assert_eq!(exit, RunExit::YieldedToUser);

        let content = tokio::fs::read_to_string(log_path).await.unwrap();
        assert!(content.contains("\"event_type\":\"TaskStarted\""));
        assert!(content.contains("\"event_type\":\"ToolExecutionStarted\""));
        assert!(content.contains("\"event_type\":\"ToolExecutionFinished\""));
        assert!(content.contains("\"event_type\":\"TaskYielded\""));

        let task_state = tokio::fs::read_to_string(dir.path().join("task_state.json"))
            .await
            .unwrap();
        assert!(task_state.contains("\"status\": \"waiting_user\""));
    }

    #[tokio::test]
    async fn test_regression_task_plan_tool_syncs_into_task_state() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let writer = Arc::new(EventLogWriter::new(log_path).await.unwrap());
        let llm: Arc<dyn LlmClient> = Arc::new(MockTaskPlanLlm {
            calls: Mutex::new(0),
        });
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(MockTaskPlanTool)];
        let context = AgentContext::new();
        let output: Arc<dyn AgentOutput> = Arc::new(TestOutput);

        let mut loop_ = AgentLoop::new(
            "cli".to_string(),
            llm,
            tools,
            context,
            output,
            Some(writer),
            dir.path().join("task_state.json"),
            None,
        );

        loop_.step("test goal".to_string()).await.unwrap();

        let task_state = tokio::fs::read_to_string(dir.path().join("task_state.json"))
            .await
            .unwrap();
        assert!(task_state.contains("\"step\": \"Inspect file\""));
        assert!(task_state.contains("\"current_step\": \"Inspect file\""));
    }

    #[tokio::test]
    async fn test_regression_large_tool_result_is_artifactized() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let writer = Arc::new(EventLogWriter::new(log_path).await.unwrap());
        let artifact_root = dir.path().join("artifacts");
        let llm: Arc<dyn LlmClient> = Arc::new(MockLargeToolLlm {
            calls: Mutex::new(0),
        });
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(MockLargeTool)];
        let context = AgentContext::new();
        let output: Arc<dyn AgentOutput> = Arc::new(TestOutput);

        let mut loop_ = AgentLoop::new(
            "cli".to_string(),
            llm,
            tools,
            context,
            output,
            Some(writer),
            dir.path().join("task_state.json"),
            Some(Arc::new(ArtifactStore::new(artifact_root.clone()))),
        );

        let _ = loop_.step("artifactize".to_string()).await.unwrap();

        let artifact_files: Vec<_> = std::fs::read_dir(artifact_root.join("cli"))
            .unwrap()
            .collect();
        assert!(!artifact_files.is_empty());

        let events = tokio::fs::read_to_string(dir.path().join("events.jsonl"))
            .await
            .unwrap();
        assert!(events.contains("\"event_type\":\"ArtifactCreated\""));
    }

    #[test]
    fn test_parse_retrieved_evidence_builds_file_and_generic_items() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("snippet.txt");
        std::fs::write(&file_path, "file-backed content").unwrap();

        let result = serde_json::json!({
            "ok": true,
            "tool_name": "search_knowledge_base",
            "output": "Found snippets",
            "data": [
                {
                    "content": "file-backed content",
                    "source": file_path.display().to_string(),
                    "relevance": 0.91
                },
                {
                    "content": "generic content",
                    "source": "memory://note-1",
                    "relevance": 0.75
                }
            ]
        })
        .to_string();

        let evidence = parse_retrieved_evidence(&result).unwrap();
        assert_eq!(evidence.len(), 2);
        assert_eq!(evidence[0].source_kind, "file");
        assert_eq!(evidence[1].source_kind, "knowledge_base");
    }
}
