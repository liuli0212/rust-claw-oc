use super::history::{ContextDiff, ContextSnapshot};
use super::model::{FileData, FunctionCall, FunctionResponse, Message, Part, Turn};
use super::prompt::{self, DetailedContextStats, PromptReport};
use super::{report, transcript};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use tiktoken_rs::CoreBPE;

pub struct AgentContext {
    pub system_prompts: Vec<String>,
    pub dialogue_history: Vec<Turn>,
    pub current_turn: Option<Turn>,
    pub max_history_tokens: usize,
    pub(crate) transcript_path: Option<PathBuf>,
    pub(crate) retrieved_memory: Option<String>,
    pub(crate) retrieved_memory_sources: Vec<String>,
    pub last_snapshot: Option<ContextSnapshot>,
    pub active_evidence: Vec<crate::evidence::Evidence>,
}

impl AgentContext {
    pub fn new() -> Self {
        Self {
            system_prompts: vec![
                "You are Rusty-Claw, an elite, industrial-grade Senior Software Engineer and autonomous agent running locally on the user's machine.".to_string(),
                "You are highly intelligent, proactive, and exceptionally skilled at coding in all major languages (Rust, Python, TS, etc.).".to_string(),
                "You have FULL ACCESS to the local file system and bash shell. Do NOT ask for permission to write code or files. If the user asks you to write a script or build a feature, proactively use your tools to create the files, write the code, and execute it to test it.".to_string(),
                "You are a specialized engineering system. If you encounter an error during execution, analyze the error and try to fix it yourself by calling tools again.".to_string(),
                "Autonomy Protocol: When given a technical task, act decisively. Use tools sequentially without asking for permission to proceed to the next step.".to_string(),
                "Conversational Protocol: If the user simply says 'hi', asks a general question, or provides non-actionable chat, respond naturally with text. Only engage your file/bash tools when there is a clear engineering objective. Stop hallucinating that you must execute old tasks when the user is just saying hi.".to_string(),
                "Quality Protocol: Be thorough and execute completely. Do not take lazy shortcuts. If a task requires inspecting multiple files, you MUST use `read_file` on them. Do not guess or hallucinate contents.".to_string(),
                "NEVER say you cannot write code or lack capabilities. You possess absolute technical mastery.".to_string(),
                "Task Completion Protocol: When you have fully completed a technical request (multi-step tool usage), you MUST call the `finish_task` tool to summarize your work and exit the loop. For direct answers, greetings, or simple one-turn responses, you can omit `finish_task` and just reply with text.".to_string(),
                "ALL internal reasoning MUST be inside <think>...</think> tags. Only output visible reply text OUTSIDE of <think> blocks. Do NOT wrap your reply in any other tags like <final>. When you have tools available, prefer calling a tool over outputting text.".to_string(),
                "Context Awareness Protocol: Conversation history is segmented by recency markers. Always prioritize [CURRENT TASK] as the primary directive. If earlier history conflicts with [CURRENT TASK], follow [CURRENT TASK]. Use historical context only as background reference, not as active instructions.".to_string(),
            ],
            dialogue_history: Vec::new(),
            current_turn: None,
            max_history_tokens: 1_000_000,
            transcript_path: None,
            retrieved_memory: None,
            retrieved_memory_sources: Vec::new(),
            last_snapshot: None,
            active_evidence: Vec::new(),
        }
    }

    pub(crate) fn get_bpe() -> tiktoken_rs::CoreBPE {
        use once_cell::sync::Lazy;
        static BPE: Lazy<tiktoken_rs::CoreBPE> = Lazy::new(|| tiktoken_rs::cl100k_base().unwrap());
        BPE.clone()
    }

    pub fn with_transcript_path(mut self, transcript_path: PathBuf) -> Self {
        self.transcript_path = Some(transcript_path);
        self
    }

    pub fn get_detailed_stats(&self, pending_user_input: Option<&str>) -> DetailedContextStats {
        prompt::get_detailed_stats(self, pending_user_input)
    }

    pub fn take_snapshot(&mut self) -> ContextSnapshot {
        let stats = self.get_detailed_stats(None);
        let system_prompt = self.build_system_prompt();
        let mut hasher = DefaultHasher::new();
        system_prompt.hash(&mut hasher);
        let hash = hasher.finish();

        let snapshot = ContextSnapshot {
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            turn_id: self
                .current_turn
                .as_ref()
                .map(|t| t.turn_id.clone())
                .unwrap_or_default(),
            stats,
            messages_count: self
                .dialogue_history
                .iter()
                .map(|t| t.messages.len())
                .sum::<usize>()
                + self
                    .current_turn
                    .as_ref()
                    .map(|t| t.messages.len())
                    .unwrap_or(0),
            system_prompt_hash: hash,
            retrieved_memory_sources: self.retrieved_memory_sources.clone(),
            history_turns_count: self.dialogue_history.len(),
        };
        self.last_snapshot = Some(snapshot.clone());
        snapshot
    }

    pub fn diff_snapshot(&self, old: &ContextSnapshot) -> ContextDiff {
        let current_stats = self.get_detailed_stats(None);
        let system_prompt = self.build_system_prompt();
        let mut hasher = DefaultHasher::new();
        system_prompt.hash(&mut hasher);
        let current_hash = hasher.finish();

        let old_sources: std::collections::HashSet<_> =
            old.retrieved_memory_sources.iter().cloned().collect();
        let new_sources_set: std::collections::HashSet<_> =
            self.retrieved_memory_sources.iter().cloned().collect();

        let new_sources = self
            .retrieved_memory_sources
            .iter()
            .filter(|s| !old_sources.contains(*s))
            .cloned()
            .collect();
        let removed_sources = old
            .retrieved_memory_sources
            .iter()
            .filter(|s| !new_sources_set.contains(*s))
            .cloned()
            .collect();

        ContextDiff {
            token_delta: current_stats.total as i64 - old.stats.total as i64,
            history_turns_delta: self.dialogue_history.len() as i32
                - old.history_turns_count as i32,
            system_prompt_changed: current_hash != old.system_prompt_hash,
            new_sources,
            removed_sources,
            memory_changed: self.retrieved_memory_sources != old.retrieved_memory_sources,
            truncated_delta: current_stats.truncated_chars as i64
                - old.stats.truncated_chars as i64,
        }
    }

    pub fn load_transcript(&mut self) -> std::io::Result<usize> {
        let Some(path) = &self.transcript_path else {
            return Ok(0);
        };
        let turns = transcript::load_turns(path)?;
        let loaded = turns.len();
        self.dialogue_history.extend(turns);
        Ok(loaded)
    }

    pub(crate) fn append_turn_to_transcript(&self, turn: &Turn) -> std::io::Result<()> {
        transcript::append_turn(self.transcript_path.as_deref(), turn)
    }

    pub(crate) fn estimate_tokens(bpe: &CoreBPE, msg: &Message) -> usize {
        let mut count = 0;
        for part in &msg.parts {
            if let Some(text) = &part.text {
                count += bpe.encode_with_special_tokens(text).len();
            }
            if let Some(fc) = &part.function_call {
                count += bpe.encode_with_special_tokens(&fc.name).len();
                count += bpe.encode_with_special_tokens(&fc.args.to_string()).len();
            }
            if let Some(fr) = &part.function_response {
                count += bpe.encode_with_special_tokens(&fr.name).len();
                count += bpe
                    .encode_with_special_tokens(&fr.response.to_string())
                    .len();
            }
        }
        count
    }

    pub(crate) fn truncate_chars(input: &str, max_chars: usize) -> String {
        if input.chars().count() <= max_chars {
            return input.to_string();
        }
        input.chars().take(max_chars).collect()
    }

    pub(crate) fn build_system_prompt(&self) -> String {
        prompt::build_system_prompt(self)
    }

    pub(crate) fn build_history_with_budget(&self) -> (Vec<Message>, usize, usize, usize) {
        super::history::build_history_with_budget(self)
    }

    pub(crate) fn turn_token_estimate(turn: &Turn, bpe: &CoreBPE) -> usize {
        turn.messages
            .iter()
            .map(|m| Self::estimate_tokens(bpe, m))
            .sum()
    }

    pub fn dialogue_history_token_estimate(&self) -> usize {
        super::history::dialogue_history_token_estimate(self)
    }

    pub fn get_context_status(&self) -> (usize, usize, usize, usize, usize) {
        super::history::get_context_status(self)
    }

    pub fn get_context_details(&self) -> String {
        report::format_context_details(self)
    }

    pub fn oldest_turns_for_compaction(&self, target_tokens: usize, min_turns: usize) -> usize {
        super::history::oldest_turns_for_compaction(self, target_tokens, min_turns)
    }

    /// Rule-based compaction: compress oldest N turns into a single structured summary Turn.
    /// No LLM call required. Preserves: user intent, tool names, key args, success/fail, errors.
    pub fn rule_based_compact(&mut self, num_turns: usize) -> Option<String> {
        super::history::rule_based_compact(self, num_turns)
    }

    pub fn start_turn(&mut self, text: String) {
        self.current_turn = Some(Turn {
            turn_id: uuid::Uuid::new_v4().to_string(),
            user_message: text.clone(),
            messages: vec![Message {
                role: "user".to_string(),
                parts: vec![Part {
                    text: Some(text),
                    function_call: None,
                    function_response: None,
                    thought_signature: None,
                    file_data: None,
                }],
            }],
        });
    }

    pub fn add_message_to_current_turn(&mut self, msg: Message) {
        if let Some(turn) = &mut self.current_turn {
            turn.messages.push(msg);
        }
    }

    pub fn compress_current_turn(&mut self, max_bytes: usize) -> usize {
        super::history::compress_current_turn(self, max_bytes)
    }

    pub fn truncate_current_turn_tool_results(&mut self, max_chars: usize) -> usize {
        super::history::truncate_current_turn_tool_results(self, max_chars)
    }

    pub fn end_turn(&mut self) {
        if let Some(turn) = self.current_turn.take() {
            if let Err(e) = self.append_turn_to_transcript(&turn) {
                tracing::warn!("Failed to append turn to transcript: {}", e);
            }
            self.dialogue_history.push(turn);
        }
    }

    pub fn build_llm_payload(
        &self,
        task_state: &crate::task_state::TaskStateSnapshot,
        assembler: &crate::context_assembler::ContextAssembler,
    ) -> (Vec<Message>, Option<Message>, PromptReport) {
        prompt::build_llm_payload(self, task_state, assembler)
    }

    pub fn format_diff(&self, diff: &ContextDiff) -> String {
        report::format_context_diff(diff)
    }

    pub fn inspect_context(&self, section: &str, arg: Option<&str>) -> String {
        report::inspect_context_section(self, section, arg)
    }

    pub(crate) fn strip_thinking_tags(text: &str) -> String {
        let mut result = text.to_string();
        while let Some(start) = result.find("<think>") {
            if let Some(end_offset) = result[start..].find("</think>") {
                let end = start + end_offset;
                let before = &result[..start];
                let after = &result[end + 8..];
                result = format!("{}{}", before, after);
            } else {
                result = result[..start].to_string();
                break;
            }
        }
        result.trim().to_string()
    }

    pub(crate) fn strip_response_payload(fr: &mut FunctionResponse) {
        let obj = match fr.response.as_object_mut() {
            Some(o) => o,
            None => return,
        };
        let result_val = match obj.get_mut("result") {
            Some(v) => v,
            None => return,
        };
        let result_str = match result_val.as_str() {
            Some(s) => s.to_string(),
            None => return,
        };

        let mut envelope: serde_json::Value = match serde_json::from_str(&result_str) {
            Ok(v) => v,
            Err(_) => {
                if result_str.len() > 500 {
                    let head: String = result_str.chars().take(200).collect();
                    *result_val = serde_json::Value::String(format!(
                        "{}\n... [stripped {} chars]",
                        head,
                        result_str.len()
                    ));
                }
                return;
            }
        };

        let env_obj = match envelope.as_object_mut() {
            Some(o) => o,
            None => return,
        };

        match fr.name.as_str() {
            "task_plan" => {
                *result_val = serde_json::Value::String("[plan updated]".to_string());
                return;
            }
            "read_file" => {
                if let Some(output) = env_obj.get_mut("output") {
                    if let Some(s) = output.as_str() {
                        let line_count = s.lines().count();
                        if line_count > 10 {
                            let head: String = s.lines().take(5).collect::<Vec<_>>().join("\n");
                            let tail: String = s
                                .lines()
                                .rev()
                                .take(5)
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .collect::<Vec<_>>()
                                .join("\n");
                            *output = serde_json::Value::String(format!(
                                "{}\n... [stripped {} lines] ...\n{}",
                                head, line_count, tail
                            ));
                        }
                    }
                }
            }
            "execute_bash" => {
                if let Some(output) = env_obj.get_mut("output") {
                    if let Some(s) = output.as_str() {
                        let char_count = s.chars().count();
                        if char_count > 500 {
                            let head: String = s.chars().take(200).collect();
                            let tail: String = s.chars().skip(char_count - 200).collect();
                            *output = serde_json::Value::String(format!(
                                "{}\n... [stripped {} chars] ...\n{}",
                                head, char_count, tail
                            ));
                        }
                    }
                }
            }
            "web_fetch" | "web_search_tavily" => {
                if let Some(output) = env_obj.get_mut("output") {
                    if let Some(s) = output.as_str() {
                        *output = serde_json::Value::String(format!(
                            "[web content stripped - {} chars]",
                            s.len()
                        ));
                    }
                }
            }
            "skill" | "use_skill" => {
                if let Some(output) = env_obj.get_mut("output") {
                    *output = serde_json::Value::String("Skill loaded.".to_string());
                }
            }
            "write_file" | "patch_file" => {}
            _ => {
                if let Some(output) = env_obj.get_mut("output") {
                    if let Some(s) = output.as_str() {
                        if s.len() > 500 {
                            let head: String = s.chars().take(200).collect();
                            let tail: String = s.chars().skip(s.chars().count() - 100).collect();
                            *output = serde_json::Value::String(format!(
                                "{}\n... [stripped {} chars] ...\n{}",
                                head,
                                s.len(),
                                tail
                            ));
                        }
                    }
                }
            }
        }

        env_obj.remove("duration_ms");
        env_obj.remove("truncated");
        env_obj.remove("recovery_attempted");
        env_obj.remove("recovery_output");
        env_obj.remove("recovery_rule");

        if let Ok(stripped_str) = serde_json::to_string(env_obj) {
            *result_val = serde_json::Value::String(stripped_str);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_context_turn_management() {
        let mut ctx = AgentContext::new();
        ctx.start_turn("Hello".to_string());
        assert!(ctx.current_turn.is_some());
        assert_eq!(ctx.current_turn.as_ref().unwrap().user_message, "Hello");

        ctx.add_message_to_current_turn(Message {
            role: "model".to_string(),
            parts: vec![Part {
                text: Some("Hi there".to_string()),
                function_call: None,
                function_response: None,
                thought_signature: None,
                file_data: None,
            }],
        });

        ctx.end_turn();
        assert!(ctx.current_turn.is_none());
        assert_eq!(ctx.dialogue_history.len(), 1);
        assert_eq!(ctx.dialogue_history[0].messages.len(), 2);
    }

    #[test]
    fn test_token_budget_truncation() {
        let mut ctx = AgentContext::new();
        ctx.max_history_tokens = 10;

        ctx.start_turn("This is a very long string that should be truncated eventually. It has many many words and will exceed fifty tokens quickly.".to_string());
        ctx.end_turn();
        ctx.start_turn("Short message".to_string());
        ctx.end_turn();

        let state = crate::task_state::TaskStateSnapshot::empty();
        let assembler = crate::context_assembler::ContextAssembler::new(100);
        let (payload, _sys, _report) = ctx.build_llm_payload(&state, &assembler);
        assert_eq!(payload.len(), 1);
        assert_eq!(
            payload.last().unwrap().parts[0].text.as_ref().unwrap(),
            "Short message"
        );
    }

    #[test]
    fn test_transcript_path_for_session_sanitizes_special_characters() {
        let dir = tempdir().unwrap();
        let path =
            transcript::transcript_path_for_session(dir.path(), "session:/with spaces?and*symbols");

        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "session__with_spaces_and_symbols.jsonl"
        );
        assert!(path.starts_with(dir.path()));
    }
}
