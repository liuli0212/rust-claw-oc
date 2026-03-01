use crate::context::{
    AgentContext, FunctionResponse, Message, Part, PromptReport, Turn, ContextSnapshot, ContextDiff
};
use crate::llm_client::{LlmClient, StreamEvent};
use crate::tools::Tool;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

#[async_trait]
pub trait AgentOutput: Send + Sync {
    async fn on_text(&self, text: &str);
    async fn on_tool_start(&self, name: &str, args: &str);
    async fn on_tool_end(&self, result: &str);
    async fn on_error(&self, error: &str);
}

pub struct AgentLoop {
    llm: Arc<dyn LlmClient>,
    tools: Vec<Arc<dyn Tool>>,
    context: AgentContext,
    output: Arc<dyn AgentOutput>,
    pub cancel_token: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RunExit {
    CompletedWithReply,
    CompletedSilent { cause: String },
    RecoverableFailed { reason: String, attempts: usize },
    HardStop { reason: String },
    YieldedToUser,
}

impl RunExit {
    pub fn label(&self) -> &'static str {
        match self {
            RunExit::CompletedWithReply => "completed_with_reply",
            RunExit::CompletedSilent { .. } => "completed_silent",
            RunExit::RecoverableFailed { .. } => "recoverable_failed",
            RunExit::HardStop { .. } => "hard_stop",
            RunExit::YieldedToUser => "yielded_to_user",
        }
    }
}

#[derive(Debug, Clone, Default)]
struct TaskState {
    goal: String,
    iterations: usize,
    consecutive_empty_responses: usize,
    energy_points: usize,
    recovery_attempts: usize,
    recovery_rule_hits: HashMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StructuredToolResult {
    ok: bool,
    tool_name: String,
    output: String,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    duration_ms: Option<u128>,
    #[serde(default)]
    truncated: bool,
    #[serde(default)]
    recovery_attempted: bool,
    #[serde(default)]
    recovery_output: Option<String>,
    #[serde(default)]
    recovery_rule: Option<String>,
}

#[derive(Debug, Clone)]
struct RecoveryRule {
    name: &'static str,
    matcher: fn(&str, &str) -> bool,
    build_command: fn(&str, &str) -> String,
}

impl AgentLoop {
    fn is_pure_final_reply(text: &str) -> bool {
        let trimmed = text.trim();
        if let Some(_final_start) = trimmed.find("<final>") {
             if let Some(final_end) = trimmed.rfind("</final>") {
                 if final_end + 8 >= trimmed.len() {
                     return true;
                 }
             }
        }
        false
    }

    const COMPACTION_TRIGGER_RATIO_NUM: usize = 80;
    const COMPACTION_TRIGGER_RATIO_DEN: usize = 100;
    const COMPACTION_TARGET_RATIO_NUM: usize = 25;
    const COMPACTION_TARGET_RATIO_DEN: usize = 100;
    const COMPACTION_MIN_TURNS: usize = 3;
    const DEFAULT_MAX_TASK_ITERATIONS: usize = 12;
    const MAX_AUTO_RECOVERY_ATTEMPTS: usize = 2;
    const MAX_LLM_RECOVERY_ATTEMPTS: usize = 25;

    pub fn new(
        llm: Arc<dyn LlmClient>,
        tools: Vec<Arc<dyn Tool>>,
        mut context: AgentContext,
        output: Arc<dyn AgentOutput>,
    ) -> Self {
        context.max_history_tokens = llm.context_window_size();
        Self {
            llm,
            tools,
            context,
            output,
            cancel_token: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn get_status(&self) -> (String, String, usize, usize) {
        let (total, max, _, _, _) = self.context.get_context_status();
        (
            self.llm.provider_name().to_string(),
            self.llm.model_name().to_string(),
            total,
            max,
        )
    }

    pub fn get_detailed_stats(&self) -> crate::context::DetailedContextStats {
        self.context.get_detailed_stats(None)
    }

    pub fn get_context_details(&self) -> String {
        self.context.get_context_details()
    }

    pub fn diff_context(&self) -> Option<ContextDiff> {
        self.context.last_snapshot.as_ref().map(|old| self.context.diff_snapshot(old))
    }

    pub fn update_llm(&mut self, new_llm: Arc<dyn LlmClient>) {
        self.context.max_history_tokens = new_llm.context_window_size();
        self.llm = new_llm;
    }

    pub async fn force_compact(&mut self) -> Result<String, String> {
        match self.maybe_compact_history(true).await {
             Some(reason) => Ok(reason),
             None => Err("Compaction failed or not needed (too few turns)".to_string())
        }
    }

    fn should_emit_prompt_report() -> bool {
        std::env::var("CLAW_PROMPT_REPORT").unwrap_or_default() == "1"
    }

    fn max_task_iterations() -> usize {
        std::env::var("CLAW_MAX_TASK_ITERATIONS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .map(|v| v.clamp(1, 50))
            .unwrap_or(Self::DEFAULT_MAX_TASK_ITERATIONS)
    }

    fn is_context_overflow_error(err: &str) -> bool {
        let lower = err.to_lowercase();
        let markers = [
            "context", "token", "too large", "exceeds", "maximum",
            "request payload size", "prompt is too long", "input too long",
        ];
        (lower.contains("400") || lower.contains("invalid argument"))
            && markers.iter().any(|m| lower.contains(m))
    }

    fn is_transient_llm_error(err: &str) -> bool {
        let lower = err.to_lowercase();
        let markers = [
            "429", "rate limit", "resource exhausted", "unavailable", "deadline",
            "timeout", "connection reset", "temporarily", "503", "502", "504",
        ];
        markers.iter().any(|m| lower.contains(m))
    }

    fn sanitize_stream_text_chunk(chunk: &str) -> (String, bool, bool) {
        let no_reply_markers = ["NO_REPLY", "<NO_REPLY/>", "<NO_REPLY>"];
        let heartbeat_markers = ["[HEARTBEAT_OK]", "<HEARTBEAT_OK/>", "HEARTBEAT_OK"];

        let mut sanitized = chunk.to_string();
        let mut saw_no_reply = false;
        let mut saw_heartbeat = false;

        for marker in no_reply_markers {
            if sanitized.contains(marker) {
                saw_no_reply = true;
                sanitized = sanitized.replace(marker, "");
            }
        }
        for marker in heartbeat_markers {
            if sanitized.contains(marker) {
                saw_heartbeat = true;
                sanitized = sanitized.replace(marker, "");
            }
        }

        (sanitized, saw_no_reply, saw_heartbeat)
    }

    async fn recover_from_llm_error(&mut self, err: &str, attempt: usize) -> bool {
        if Self::is_context_overflow_error(err) {
            if attempt > 3 {
                return false;
            }
            self.output
                .on_text("[System] LLM context overflow detected. Running compaction...\n")
                .await;

            let _ = self.maybe_compact_history(true).await;
            let truncated = self.context.truncate_current_turn_tool_results(20_000);
            if truncated > 0 {
                self.output.on_text(&format!("[System] Truncated {} tool result(s).\n", truncated)).await;
            }
            self.context.set_retrieved_memory(None, Vec::new());
            return true;
        }

        if Self::is_transient_llm_error(err) && attempt < Self::MAX_LLM_RECOVERY_ATTEMPTS {
            let exponent = (attempt as u32).min(6);
            let backoff_ms = 500u64.saturating_mul(2u64.pow(exponent));
            self.output.on_text(&format!("[System] Transient error. Retrying in {} ms...\n", backoff_ms)).await;
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            return true;
        }
        false
    }

    fn rewrite_memory_query(query: &str) -> String {
        let trimmed = query.trim();
        if trimmed.is_empty() { return String::new(); }
        let mut keywords = Vec::new();
        for token in trimmed.split(|c: char| !c.is_alphanumeric()) {
            let lower = token.to_lowercase();
            if token.len() >= 3 {
                if !keywords.iter().any(|k| k == &lower) { keywords.push(lower); }
            }
            if keywords.len() >= 8 { break; }
        }
        if keywords.is_empty() { return trimmed.to_string(); }
        format!("{trimmed}\nkeywords: {}", keywords.join(" "))
    }

    async fn execute_retrieval_task(
        tools: Vec<Arc<dyn Tool>>,
        query: String,
        output: Arc<dyn AgentOutput>,
    ) -> (String, Vec<String>) {
        if query.trim().is_empty() { return (String::new(), Vec::new()); }
        let maybe_tool = tools.iter().find(|t| t.name() == "search_knowledge_base").cloned();
        let Some(tool) = maybe_tool else { return (String::new(), Vec::new()); };

        let start = Instant::now();
        let result = tool.execute(serde_json::json!({ "query": Self::rewrite_memory_query(&query), "limit": 3 })).await;
        let elapsed = start.elapsed().as_millis();
        if elapsed >= 1000 {
            output.on_text(&format!("[System] Memory retrieval finished in {} ms.\n", elapsed)).await;
        }

        match result {
            Ok(text) if !text.contains("No relevant information") => {
                let mut sources = Vec::new();
                for line in text.lines() {
                    if let Some(rest) = line.strip_prefix("--- Source: ") {
                        let src = rest.split(" (Relevance:").next().unwrap_or(rest).trim().to_string();
                        if !src.is_empty() && !sources.contains(&src) { sources.push(src); }
                    }
                }
                (text, sources)
            }
            _ => (String::new(), Vec::new()),
        }
    }

    async fn maybe_compact_history(&mut self, force: bool) -> Option<String> {
        let (current, max, _, _, _) = self.context.get_context_status();
        let trigger = max.saturating_mul(Self::COMPACTION_TRIGGER_RATIO_NUM) / Self::COMPACTION_TRIGGER_RATIO_DEN;
        
        if !force && current < trigger { return None; }

        let target = max.saturating_mul(Self::COMPACTION_TARGET_RATIO_NUM) / Self::COMPACTION_TARGET_RATIO_DEN;
        let to_compact = self.context.oldest_turns_for_compaction(target, Self::COMPACTION_MIN_TURNS);
        if to_compact == 0 { return None; }

        let start = Instant::now();
        let turns_to_process: Vec<Turn> = self.context.dialogue_history.drain(0..to_compact).collect();
        let mut summary_input = String::new();
        for t in &turns_to_process {
            summary_input.push_str(&format!("Turn: {}\n", t.user_message));
        }

        let prompt = format!(
            "You are a memory compaction module. Summarize the following dialogue history concisely while preserving ALL critical technical facts, file paths, bug causes, and decisions made. Return ONLY the summary.\n\n{}",
            summary_input
        );

        match self.llm.generate_text(vec![Message {
            role: "user".to_string(),
            parts: vec![Part {
                text: Some(prompt),
                function_call: None,
                function_response: None,
                thought_signature: None,
            }],
        }], None).await {
            Ok(summary) => {
                let summary_turn = Turn {
                    turn_id: format!("compact-{}", uuid::Uuid::new_v4()),
                    user_message: "[SYSTEM] History Compacted".to_string(),
                    messages: vec![Message {
                        role: "user".to_string(),
                        parts: vec![Part {
                            text: Some(format!("Summary of earlier interactions: {}", summary)),
                            function_call: None, function_response: None, thought_signature: None,
                        }],
                    }],
                };
                self.context.dialogue_history.insert(0, summary_turn);
                let reason = format!("Compacted {} turns into a summary ({}ms)", to_compact, start.elapsed().as_millis());
                self.output.on_text(&format!("[System] {}\n", reason)).await;
                Some(reason)
            }
            Err(e) => {
                self.context.dialogue_history.splice(0..0, turns_to_process);
                tracing::error!("Compaction failed: {}", e);
                None
            }
        }
    }

    fn parse_structured_tool_result(tool_name: &str, raw: &str) -> StructuredToolResult {
        if let Ok(mut parsed) = serde_json::from_str::<StructuredToolResult>(raw) {
            if parsed.tool_name.is_empty() {
                parsed.tool_name = tool_name.to_string();
            }
            return parsed;
        }

        StructuredToolResult {
            ok: !raw.to_lowercase().contains("error"),
            tool_name: tool_name.to_string(),
            output: raw.to_string(),
            exit_code: None,
            duration_ms: None,
            truncated: false,
            recovery_attempted: false,
            recovery_output: None,
            recovery_rule: None,
        }
    }

    fn recovery_rules() -> Vec<RecoveryRule> {
        fn missing_command_match(_cmd: &str, output: &str) -> bool {
            output.to_lowercase().contains("command not found")
        }
        fn missing_command_fix(command: &str, _output: &str) -> String {
            let cmd = command.split_whitespace().next().unwrap_or_default().trim().to_string();
            format!("command -v {0} || which {0} || echo 'missing command: {0}'", cmd)
        }
        fn missing_path_match(_cmd: &str, output: &str) -> bool {
            output.to_lowercase().contains("no such file or directory")
        }
        fn missing_path_fix(command: &str, _output: &str) -> String {
            let mut parts = command.split_whitespace();
            if let Some(head) = parts.next() {
                if head == "cat" {
                    if let Some(path) = parts.next() {
                        let parent = Path::new(path).parent().map(|p| p.display().to_string()).unwrap_or_else(|| ".".to_string());
                        return format!("pwd && ls -la {}", parent);
                    }
                }
            }
            "pwd && ls -la".to_string()
        }
        fn cargo_toml_match(_cmd: &str, output: &str) -> bool {
            output.to_lowercase().contains("could find `cargo.toml`")
        }
        fn cargo_toml_fix(_cmd: &str, _output: &str) -> String {
            "pwd && ls -la && find .. -maxdepth 3 -name Cargo.toml".to_string()
        }

        vec![
            RecoveryRule { name: "missing_command", matcher: missing_command_match, build_command: missing_command_fix },
            RecoveryRule { name: "missing_path", matcher: missing_path_match, build_command: missing_path_fix },
            RecoveryRule { name: "missing_cargo_toml", matcher: cargo_toml_match, build_command: cargo_toml_fix },
        ]
    }

    async fn execute_tool_call_with_recovery(
        &self,
        tool_name: &str,
        tool_args: &Value,
        task_state: &mut TaskState,
    ) -> StructuredToolResult {
        let Some(tool) = self.tools.iter().find(|t| t.name() == tool_name) else {
            return StructuredToolResult { ok: false, tool_name: tool_name.to_string(), output: format!("Error: Tool '{}' not found", tool_name), exit_code: None, duration_ms: None, truncated: false, recovery_attempted: false, recovery_output: None, recovery_rule: None };
        };

        let raw = match tool.execute(tool_args.clone()).await { Ok(res) => res, Err(e) => format!("Error: {}", e) };
        let mut parsed = Self::parse_structured_tool_result(tool_name, &raw);
        if parsed.ok || tool_name != "execute_bash" { return parsed; }

        if task_state.recovery_attempts >= Self::MAX_AUTO_RECOVERY_ATTEMPTS { return parsed; }

        let original_command = tool_args.get("command").and_then(|v| v.as_str()).unwrap_or_default();
        for rule in Self::recovery_rules() {
            if (rule.matcher)(original_command, &parsed.output) {
                let recovery_command = (rule.build_command)(original_command, &parsed.output);
                task_state.recovery_attempts += 1;
                *task_state.recovery_rule_hits.entry(rule.name.to_string()).or_insert(0) += 1;
                let recovery_raw = match tool.execute(serde_json::json!({ "command": recovery_command, "timeout": 20 })).await { Ok(res) => res, Err(e) => format!("Error: {}", e) };
                let recovery_parsed = Self::parse_structured_tool_result("execute_bash", &recovery_raw);
                parsed.recovery_attempted = true;
                parsed.recovery_output = Some(recovery_parsed.output.clone());
                parsed.recovery_rule = Some(rule.name.to_string());
                if recovery_parsed.ok { parsed.ok = true; }
                return parsed;
            }
        }
        parsed
    }

    pub async fn step(&mut self, goal: String) -> Result<RunExit, Box<dyn std::error::Error + Send + Sync>> {
        self.context.take_snapshot();
        // Reset cancel token at the start of each turn
        self.cancel_token.store(false, Ordering::SeqCst);

        let mut task_state = TaskState {
            goal: goal.clone(),
            energy_points: 20,
            ..Default::default()
        };

        self.context.start_turn(goal);

        while task_state.energy_points > 0 {
            if self.cancel_token.load(Ordering::SeqCst) {
                self.output.on_text("\n\x1b[33m[System] Task cancelled by user.\x1b[0m\n").await;
                return Ok(RunExit::HardStop { reason: "cancelled_by_user".to_string() });
            }

            task_state.iterations += 1;
            task_state.energy_points = task_state.energy_points.saturating_sub(1);

            if task_state.iterations > Self::max_task_iterations() {
                return Ok(RunExit::RecoverableFailed { reason: "max_iterations".to_string(), attempts: task_state.iterations });
            }

            let (mem, sources) = Self::execute_retrieval_task(self.tools.clone(), task_state.goal.clone(), self.output.clone()).await;
            self.context.set_retrieved_memory(if mem.is_empty() { None } else { Some(mem) }, sources);

            let _ = self.maybe_compact_history(false).await;

            let (messages, system, report) = self.context.build_llm_payload();
            if Self::should_emit_prompt_report() {
                self.output.on_text(&format!("\n[Prompt Report] history={} turns_inc={} cur={} sys={} total={}\n", 
                    report.history_tokens_used, report.history_turns_included, report.current_turn_tokens, report.system_prompt_tokens, report.total_prompt_tokens)).await;
            }

            let mut llm_attempts = 0;
            let mut full_text = String::new();
            let mut tool_calls = Vec::new();

            loop {
                llm_attempts += 1;
                match self.llm.stream(messages.clone(), system.clone(), self.tools.clone()).await {
                    Ok(mut rx) => {
                        while let Some(event) = rx.recv().await {
                            match event {
                                StreamEvent::Text(chunk) => {
                                    let (clean, _, _) = Self::sanitize_stream_text_chunk(&chunk);
                                    if !clean.is_empty() {
                                        full_text.push_str(&clean);
                                        self.output.on_text(&clean).await;
                                    }
                                }
                                StreamEvent::ToolCall(call, _) => {
                                    let sig = format!("{}:{}", call.name, call.args.to_string());
                                    tool_calls.push((call, sig));
                                }
                                StreamEvent::Error(e) => {
                                    if self.recover_from_llm_error(&e, llm_attempts).await { continue; }
                                    self.output.on_error(&format!("[LLM Error]: {}", e)).await;
                                    break;
                                }
                                _ => {}
                            }
                        }
                        break;
                    }
                    Err(e) => {
                        if self.recover_from_llm_error(&e.to_string(), llm_attempts).await { continue; }
                        self.output.on_error(&format!("[LLM Error]: {}", e)).await;
                        break;
                    }
                }
            }

            if !full_text.is_empty() {
                self.context.add_message_to_current_turn(Message {
                    role: "model".to_string(),
                    parts: vec![Part { text: Some(full_text.clone()), function_call: None, function_response: None, thought_signature: None }],
                });
            }

            if tool_calls.is_empty() {
                if Self::is_pure_final_reply(&full_text) { return Ok(RunExit::CompletedWithReply); }
                if full_text.trim().is_empty() {
                     if task_state.consecutive_empty_responses > 10 { return Ok(RunExit::YieldedToUser); }
                     task_state.consecutive_empty_responses += 1;
                     tokio::time::sleep(Duration::from_millis(1000)).await;
                     continue;
                }
                // Relaxed: Just log and yield for now if no tool call, instead of hard stop.
                tracing::info!("Agent returned text without tool call. Yielding to user.");
                return Ok(RunExit::YieldedToUser);
            }

            let mut response_parts = Vec::new();
            let mut requested_finish = false;
            let mut executed_signatures = HashSet::new();

            for (call, sig) in tool_calls {
                if !executed_signatures.insert(sig) || call.name.trim().is_empty() { continue; }
                if call.name == "finish_task" {
                    requested_finish = true;
                    // Delete the task plan file when task is officially finished
                    let _ = std::fs::remove_file(".rusty_claw_task_plan.json");
                    let mut summary = call.args.to_string();
                    if let Some(obj) = call.args.as_object() {
                        if let Some(s) = obj.get("summary").and_then(|v| v.as_str()) { summary = s.to_string(); }
                    }
                    self.output.on_text(&format!("\n\x1b[35m[Agent]: {}\x1b[0m\n", summary)).await;
                    break;
                }

                self.output.on_tool_start(&call.name, &call.args.to_string()).await;
                let tool_result = self.execute_tool_call_with_recovery(&call.name, &call.args, &mut task_state).await;
                if tool_result.ok { task_state.energy_points = (task_state.energy_points + 2).min(60); }
                self.output.on_tool_end(&tool_result.output).await;

                response_parts.push(Part {
                    text: None, function_call: None, thought_signature: None,
                    function_response: Some(FunctionResponse { name: call.name, response: serde_json::json!({ "result": tool_result.output }), tool_call_id: call.id }),
                });
            }

            if requested_finish { return Ok(RunExit::CompletedWithReply); }
            self.context.add_message_to_current_turn(Message { role: "function".to_string(), parts: response_parts });
        }

        self.context.end_turn();
        Ok(RunExit::YieldedToUser)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    
    struct MockOutput;
    #[async_trait]
    impl AgentOutput for MockOutput {
        async fn on_text(&self, _text: &str) {}
        async fn on_tool_start(&self, _name: &str, _args: &str) {}
        async fn on_tool_end(&self, _result: &str) {}
        async fn on_error(&self, _error: &str) {}
    }

    struct MockLlm;
    #[async_trait]
    impl LlmClient for MockLlm {
        fn provider_name(&self) -> &str { "mock" }
        fn model_name(&self) -> &str { "mock" }
        fn context_window_size(&self) -> usize { 1000 }
        async fn generate_text(&self, _m: Vec<Message>, _s: Option<Message>, _t: Option<Vec<Arc<dyn Tool>>>) -> Result<String, Box<dyn std::error::Error + Send + Sync>> { Ok("done".to_string()) }
        async fn stream(&self, _m: Vec<Message>, _s: Option<Message>, _t: Vec<Arc<dyn Tool>>) -> Result<tokio::sync::mpsc::Receiver<StreamEvent>, Box<dyn std::error::Error + Send + Sync>> {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let _ = tx.send(StreamEvent::Text("done".to_string())).await;
            Ok(rx)
        }
    }

    #[tokio::test]
    async fn test_cancel_token_reset() {
        let llm = Arc::new(MockLlm);
        let output = Arc::new(MockOutput);
        let context = AgentContext::new();
        let mut agent = AgentLoop::new(llm, vec![], context, output);
        
        // Simulate cancelled state
        agent.cancel_token.store(true, Ordering::SeqCst);
        
        // Execute step
        let _ = agent.step("test".to_string()).await;
        
        // Verify reset
        assert!(!agent.cancel_token.load(Ordering::SeqCst));
    }
}
