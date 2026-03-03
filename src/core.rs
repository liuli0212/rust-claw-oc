use crate::context::{
    AgentContext, FunctionResponse, Message, Part, Turn, ContextDiff
};
use crate::llm_client::{LlmClient, StreamEvent};
use crate::tools::Tool;
use crate::utils::truncate_log;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicBool, Ordering};

#[async_trait]
pub trait AgentOutput: Send + Sync {
    async fn on_text(&self, text: &str);
    async fn on_tool_start(&self, name: &str, args: &str);
    async fn on_tool_end(&self, result: &str);
    async fn on_error(&self, error: &str);
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
    goal: String,
    iterations: usize,
    recovery_attempts: usize,
    recovery_rule_hits: HashMap<String, usize>,
    consecutive_empty_responses: usize,
    energy_points: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskPlan {
    steps: Vec<String>,
    current_step_index: usize,
}

pub struct AgentLoop {
    llm: Arc<dyn LlmClient>,
    tools: Vec<Arc<dyn Tool>>,
    context: AgentContext,
    output: Arc<dyn AgentOutput>,
    pub cancel_token: Arc<AtomicBool>,
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
            cancel_token: Arc::new(AtomicBool::new(false)),
        }
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

    async fn maybe_compact_history(&mut self, force: bool) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (current_usage, max_tokens, _, _, _) = self.context.get_context_status();
        let threshold = (max_tokens as f64 * 0.85) as usize;

        if force || current_usage > threshold {
            tracing::info!("Compacting history... (usage={}, threshold={})", current_usage, threshold);
            
            // Try summarization first if we have a summarizer model available (not implemented yet)
            // For now, simple truncation happens automatically in build_llm_payload via build_history_with_budget
            
            // Just force a rebuild to ensure we are within limits before next turn
            let (new_usage, _, _, _, _) = self.context.get_context_status();
            tracing::info!("History compaction check done. Usage: {}", new_usage);
        }
        Ok(())
    }

    pub fn get_context_details(&self) -> String {
        self.context.get_context_details()
    }

    pub fn diff_snapshot(&self) -> Option<crate::context::ContextDiff> {
        if let Some(last) = &self.context.last_snapshot {
             Some(self.context.diff_snapshot(last))
        } else {
             None
        }
    }

    pub fn format_diff(&self, diff: &crate::context::ContextDiff) -> String {
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
        Ok("Compacted".to_string())
    }

    pub fn get_detailed_stats(&self) -> crate::context::DetailedContextStats {
        self.context.get_detailed_stats(None)
    }

    pub fn update_llm(&mut self, llm: Arc<dyn LlmClient>) {
        self.llm = llm;
    }


    fn is_transient_llm_error(err: &crate::llm_client::LlmError) -> bool {
        // Simple heuristic for now. Network errors or 5xx are usually transient.
        // We can inspect the error string or add more types to LlmError.
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
        // self.context.take_snapshot(); // Snapshot functionality removed or refactored in Context
        self.cancel_token.store(false, Ordering::SeqCst);

        let mut task_state = TaskState {
            goal: goal.clone(),
            iterations: 0,
            recovery_attempts: 0,
            recovery_rule_hits: HashMap::new(),
            consecutive_empty_responses: 0,
            energy_points: Self::INITIAL_ENERGY,
        };

        // Add user goal to context if not already present or if it's a new turn
        self.context.start_turn(goal.clone());

        loop {
            // Check cancellation
            if self.cancel_token.load(Ordering::SeqCst) {
                return Ok(RunExit::StoppedByUser);
            }

            if task_state.iterations >= Self::MAX_ITERATIONS {
                return Ok(RunExit::AgentTurnLimitReached);
            }
            task_state.iterations += 1;
            task_state.energy_points = task_state.energy_points.saturating_sub(1);

            if task_state.energy_points == 0 {
                 self.output.on_text("[System] Energy depleted. Stopping to prevent infinite loops.").await;
                 return Ok(RunExit::CriticallyFailed("Energy depleted".to_string()));
            }

            let _ = self.maybe_compact_history(false).await;

            let (messages, system, _) = self.context.build_llm_payload();

            let mut llm_attempts = 0;
            let mut full_text = String::new();
            let mut tool_calls_accumulated = Vec::new();

            loop {
                llm_attempts += 1;
                // Streaming response
                let stream_result = self.llm.stream(messages.clone(), system.clone(), self.tools.clone()).await;
                
                match stream_result {
                    Ok(mut rx) => {
                        let mut current_turn_text = String::new();
                        while let Some(event) = rx.recv().await {
                            if self.cancel_token.load(Ordering::SeqCst) {
                                return Ok(RunExit::StoppedByUser);
                            }
                            match event {
                                StreamEvent::Text(t) => {
                                    self.output.on_text(&t).await;
                                    current_turn_text.push_str(&t);
                                }
                                StreamEvent::Thought(_) => {} // Ignore thought updates for now
                                StreamEvent::ToolCall(tc, _) => {
                                    tool_calls_accumulated.push(tc);
                                }
                                StreamEvent::Done => break,
                                StreamEvent::Error(e) => {
                                    self.output.on_error(&format!("Stream error: {}", e)).await;
                                }
                            }
                        }
                        full_text = current_turn_text;
                        break; // Success
                    },
                    Err(e) => {
                        if !self.handle_llm_error(&e, llm_attempts).await {
                             return Err(Box::new(e));
                        }
                    }
                }
            }

            // Check if empty response
            if full_text.trim().is_empty() && tool_calls_accumulated.is_empty() {
                task_state.consecutive_empty_responses += 1;
                if task_state.consecutive_empty_responses >= Self::MAX_CONSECUTIVE_EMPTY_RESPONSES {
                     return Ok(RunExit::CriticallyFailed("Too many empty responses".to_string()));
                }
                continue; 
            } else {
                task_state.consecutive_empty_responses = 0;
            }

            // Construct model message
            let mut model_parts = Vec::new();
            if !full_text.is_empty() {
                model_parts.push(Part {
                    text: Some(full_text.clone()),
                    function_call: None,
                    function_response: None,
                    thought_signature: None,
                });
            }
            for tc in &tool_calls_accumulated {
                model_parts.push(Part {
                    text: None,
                    function_call: Some(tc.clone()),
                    function_response: None,
                    thought_signature: None,
                });
            }
            self.context.add_message_to_current_turn(Message {
                role: "model".to_string(),
                parts: model_parts,
            });

            // Execute tools
            let mut requested_finish = false;
            let mut executed_signatures = HashSet::new();
            
            // If no tools called but text was generated, just yield to user
            if tool_calls_accumulated.is_empty() {
                // If the model just chats, we yield to user to reply
                // But first check if it looks like a completion or question
                tracing::info!("Agent returned text without tool call. Yielding to user.");
                
                // End the turn before yielding
                self.context.end_turn();
                return Ok(RunExit::YieldedToUser);
            }

            for call in tool_calls_accumulated {
                 // Check deduplication
                let sig = format!("{}:{}", call.name, call.args);
                if !executed_signatures.insert(sig) || call.name.trim().is_empty() { continue; }
                
                if call.name == "finish_task" {
                    requested_finish = true;
                    // Delete the task plan file when task is officially finished
                    let _ = std::fs::remove_file(".rusty_claw_task_plan.json");
                    let mut summary = call.args.to_string();
                    if let Some(obj) = call.args.as_object() {
                        if let Some(s) = obj.get("summary").and_then(|v| v.as_str()) {
                            summary = s.to_string();
                        }
                    }
                    self.context.end_turn();
                    return Ok(RunExit::Finished(summary));
                }

                // Find tool
                let tool_opt = self.tools.iter().find(|t| t.name() == call.name);
                if let Some(tool) = tool_opt {
                    self.output.on_tool_start(&call.name, &call.args.to_string()).await;
                    
                    let result = match tool.execute(call.args.clone()).await {
                        Ok(res) => res,
                        Err(e) => format!("Error executing {}: {}", call.name, e),
                    };

                    self.output.on_tool_end(&result).await;
                    
                    // Create function response part
                    let response_part = Part {
                        text: None,
                        function_call: None,
                        function_response: Some(crate::context::FunctionResponse {
                            name: call.name.clone(),
                            response: serde_json::json!({ "result": result }),
                            tool_call_id: call.id.clone(),
                        }),
                        thought_signature: None,
                    };
                    self.context.add_message_to_current_turn(Message {
                        role: "function".to_string(),
                        parts: vec![response_part],
                    });

                } else {
                    let err_msg = format!("Tool not found: {}", call.name);
                    self.output.on_error(&err_msg).await;
                    let error_part = Part {
                        text: None,
                        function_call: None,
                        function_response: Some(crate::context::FunctionResponse {
                            name: call.name.clone(),
                            response: serde_json::json!({ "error": err_msg }),
                            tool_call_id: call.id.clone(),
                        }),
                        thought_signature: None,
                    };
                     self.context.add_message_to_current_turn(Message {
                        role: "function".to_string(),
                        parts: vec![error_part],
                    });
                }
            }

            if requested_finish {
                self.context.end_turn();
                break;
            }
        }

        Ok(RunExit::Finished("Loop ended".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use crate::llm_client::{LlmClient, StreamEvent, LlmError};
    use crate::context::{Message, Part};
    use std::sync::atomic::AtomicBool;

    struct MockLlm;

    #[async_trait]
    impl LlmClient for MockLlm {
        fn model_name(&self) -> &str { "mock" }
        fn provider_name(&self) -> &str { "mock" }
        fn context_window_size(&self) -> usize { 1000 }
        async fn generate_text(&self, _m: Vec<Message>, _s: Option<Message>) -> Result<String, LlmError> {
            Ok("mock response".to_string())
        }
        async fn stream(&self, _m: Vec<Message>, _s: Option<Message>, _t: Vec<Arc<dyn Tool>>) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
            let (tx, rx) = mpsc::channel(1);
            let _ = tx.send(StreamEvent::Text("mock response".to_string())).await;
            let _ = tx.send(StreamEvent::Done).await;
            Ok(rx)
        }
    }

    struct MockOutput;
    #[async_trait]
    impl AgentOutput for MockOutput {
        async fn on_text(&self, _t: &str) {}
        async fn on_tool_start(&self, _n: &str, _a: &str) {}
        async fn on_tool_end(&self, _r: &str) {}
        async fn on_error(&self, _e: &str) {}
    }

    #[tokio::test]
    async fn test_cancel_token_reset() {
        let mut ctx = AgentContext::new();
        let llm = Arc::new(MockLlm);
        let output = Arc::new(MockOutput);
        let mut agent = AgentLoop::new(llm, vec![], ctx, output);
        
        // Simulating a previous cancellation
        agent.cancel_token.store(true, Ordering::SeqCst);
        
        // Starting a new step should reset it
        let _ = agent.step("test goal".to_string()).await;
        
        assert_eq!(agent.cancel_token.load(Ordering::SeqCst), false);
    }
}
