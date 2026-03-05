use crate::context::{
    AgentContext, FunctionResponse, Message, Part, ContextDiff
};
use crate::llm_client::{LlmClient, StreamEvent};
use crate::tools::Tool;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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
    llm: Arc<dyn LlmClient>,
    tools: Vec<Arc<dyn Tool>>,
    context: AgentContext,
    output: Arc<dyn AgentOutput>,
    pub cancel_token: Arc<Notify>,
    pub cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl AgentLoop {
    const MAX_LLM_RECOVERY_ATTEMPTS: usize = 3;
    const MAX_CONSECUTIVE_EMPTY_RESPONSES: usize = 3;
    const MAX_ITERATIONS: usize = 25;
    const INITIAL_ENERGY: usize = 100;

    pub fn new(
        llm: Arc<dyn LlmClient>,
        tools: Vec<Arc<dyn Tool>>,
        context: AgentContext,
        output: Arc<dyn AgentOutput>,
    ) -> Self {
        Self {
            llm,
            tools,
            context,
            output,
            cancel_token: Arc::new(Notify::new()),
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    pub fn request_cancel(&self) {
        self.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        self.cancel_token.notify_waiters();
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
        self.context.last_snapshot.as_ref().map(|old| self.context.diff_snapshot(old))
    }

    pub fn format_diff(&self, diff: &ContextDiff) -> String {
        self.context.format_diff(diff)
    }

    pub fn inspect_context(&self, section: &str, arg: Option<&str>) -> String {
        self.context.inspect_context(section, arg)
    }
    
    pub fn build_llm_payload(&self) -> (Vec<Message>, Option<Message>, crate::context::PromptReport) {
        self.context.build_llm_payload()
    }

    pub async fn force_compact(&mut self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        self.maybe_compact_history(true).await?;
        Ok("Compaction triggered.".to_string())
    }

    async fn maybe_compact_history(&mut self, force: bool) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (current_usage, max_tokens, _, _, _) = self.context.get_context_status();
        let threshold = (max_tokens as f64 * 0.85) as usize;

        if !force && current_usage <= threshold {
            return Ok(());
        }

        // Target: free up ~30% of max tokens worth of history
        let target_tokens = max_tokens.saturating_mul(30) / 100;
        let min_turns = 2;
        let num_to_compact = self.context.oldest_turns_for_compaction(target_tokens, min_turns);

        if num_to_compact == 0 {
            return Ok(());
        }

        tracing::info!("Compacting {} oldest turns (usage={}, threshold={})", num_to_compact, current_usage, threshold);

        if let Some(reason) = self.context.rule_based_compact(num_to_compact) {
            self.output.on_text(&format!("[System] {}\n", reason)).await;
        }

        Ok(())
    }

    fn is_transient_llm_error(err: &crate::llm_client::LlmError) -> bool {
        let msg = format!("{}", err).to_lowercase();
        msg.contains("timeout") || msg.contains("500") || msg.contains("502") || msg.contains("503") || msg.contains("rate limit")
    }

    async fn handle_llm_error(&self, err: &crate::llm_client::LlmError, attempt: usize) -> bool {
        if Self::is_transient_llm_error(err) && attempt < Self::MAX_LLM_RECOVERY_ATTEMPTS {
            let exponent = (attempt as u32).min(6);
            let backoff_ms = 500u64.saturating_mul(2u64.pow(exponent));
            self.output.on_text(&format!("[System] Transient error. Retrying in {} ms... (Attempt {}/{})\n", backoff_ms, attempt, Self::MAX_LLM_RECOVERY_ATTEMPTS)).await;
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            return true;
        }
        false
    }

    pub async fn step(&mut self, goal: String) -> Result<RunExit, Box<dyn std::error::Error + Send + Sync>> {
        // Reset cancel flag at start of each step
        self.cancelled.store(false, std::sync::atomic::Ordering::SeqCst);
        self.context.take_snapshot();

        let mut task_state = TaskState {
            iterations: 0,
            energy_points: Self::INITIAL_ENERGY,
        };

        let mut compaction_checked = false;
        let mut consecutive_empty_responses = 0;

        self.context.start_turn(goal);

        loop {
            // Check persistent cancel flag at top of each iteration
            if self.is_cancelled() {
                self.context.end_turn();
                return Ok(RunExit::StoppedByUser);
            }
            if task_state.iterations >= Self::MAX_ITERATIONS {
                tracing::warn!("Agent loop reached MAX_ITERATIONS ({}). Exiting to prevent runaway loops.", Self::MAX_ITERATIONS);
                self.context.end_turn();
                return Ok(RunExit::AgentTurnLimitReached);
            }
            task_state.iterations += 1;
            task_state.energy_points = task_state.energy_points.saturating_sub(1);

            if task_state.energy_points == 0 {
                 tracing::error!("Energy points depleted.");
                 self.output.on_text("[System] Energy depleted. Stopping to prevent infinite loops.").await;
                 self.context.end_turn();
                 return Ok(RunExit::CriticallyFailed("Energy depleted".to_string()));
            }

            if !compaction_checked {
                let _ = self.maybe_compact_history(false).await;
                compaction_checked = true;
            }

            let (messages, system, _) = self.context.build_llm_payload();

            let mut llm_attempts = 0;
            let mut tool_calls_accumulated: Vec<(crate::context::FunctionCall, Option<String>)> = Vec::new();

            let full_text = loop {
                llm_attempts += 1;
                
                let stream_res = tokio::select! {
                    res = self.llm.stream(messages.clone(), system.clone(), self.tools.clone()) => res,
                    _ = self.cancel_token.notified() => {
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
                            self.context.end_turn();
                            return Ok(exit);
                        }

                        break current_turn_text;
                    },
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
                     self.context.end_turn();
                     return Ok(RunExit::CriticallyFailed("Too many empty responses".to_string()));
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
                if !executed_signatures.insert(sig) || call.name.trim().is_empty() { continue; }
                
                if call.name == "finish_task" {
                    let _ = std::fs::remove_file(".rusty_claw_task_plan.json");
                    let mut summary = call.args.to_string();
                    if let Some(obj) = call.args.as_object() {
                        if let Some(s) = obj.get("summary").and_then(|v| v.as_str()) {
                            summary = s.to_string();
                        }
                    }
                    self.output.flush().await;
                    self.output.on_text(&format!("\n{}\n", summary)).await;
                    self.output.flush().await;
                    self.context.end_turn();
                    return Ok(RunExit::Finished(summary));
                }

                let tool_opt = self.tools.iter().find(|t| t.name() == call.name);
                let (result, is_error, stopped) = if let Some(tool) = tool_opt {
                    self.output.flush().await;
                    self.output.on_tool_start(&call.name, &call.args.to_string()).await;
                    
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
                    self.output.on_error(&result).await;
                    stop_loop = true;
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
                    }
                }

                self.context.add_message_to_current_turn(Message {
                    role: "function".to_string(),
                    parts: vec![Part {
                        text: None,
                        function_call: None,
                        function_response: Some(FunctionResponse {
                            name: call.name.clone(),
                            response: serde_json::json!({ "result": result }),
                            tool_call_id: call.id.clone(),
                        }),
                        thought_signature: None,
                    }],
                });
            }

            if stop_loop {
                 self.context.end_turn();
                 return Ok(RunExit::StoppedByUser);
            }
        }
    }
}
