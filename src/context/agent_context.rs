use super::history::{ContextDiff, ContextSnapshot};
use super::model::{FunctionResponse, Message, Turn};
use super::prompt::{self, DetailedContextStats, PromptReport};
use super::{report, sanitize, state, token, transcript, turns};
use std::path::PathBuf;
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
    /// Injected by SkillRuntime via before_prompt_build hook.
    pub skill_contract: Option<String>,
    pub skill_instructions: Option<String>,
    pub skill_state_summary: Option<String>,
    pub execution_notices: Option<String>,
}

impl Default for AgentContext {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentContext {
    pub fn new() -> Self {
        Self::with_system_prompts(vec![
            "You are Rusty-Claw, an elite, industrial-grade Senior Software Engineer and autonomous agent running locally on the user's machine.".to_string(),
            "You are highly intelligent, proactive, and exceptionally skilled at coding in all major languages (Rust, Python, TS, etc.).".to_string(),
            "You have FULL ACCESS to the local file system and bash shell. Do NOT ask for permission to write code or files. If the user asks you to write a script or build a feature, proactively use your tools to create the files, write the code, and execute it to test it.".to_string(),
            "You are a specialized engineering system. If you encounter an error during execution, analyze the error and try to fix it yourself by calling tools again.".to_string(),
            "Autonomy Protocol: When given a technical task, act decisively. Use tools without asking for permission to proceed to the next step.".to_string(),
            "Concurrency Protocol: You MUST issue multiple tool calls concurrently in a single response whenever actions are independent. ONLY wait for a response if Tool B strictly depends on Tool A.".to_string(),
            "Conversational Protocol: If the user simply says 'hi', asks a general question, or provides non-actionable chat, respond naturally with text. Only engage your file/bash tools when there is a clear engineering objective. Stop hallucinating that you must execute old tasks when the user is just saying hi.".to_string(),
            "Quality Protocol: Be thorough and execute completely. Do not take lazy shortcuts. If a task requires inspecting multiple files, you MUST use `read_file` on them. Do not guess or hallucinate contents.".to_string(),
            "NEVER say you cannot write code or lack capabilities. You possess absolute technical mastery.".to_string(),
            "Task Completion Protocol: When you have fully completed the user's request, stop calling tools and provide the complete final answer as normal visible text. A final text response with no tool calls ends the execution loop. Do not call a completion tool; there is no completion tool.".to_string(),
            "ALL internal reasoning MUST be inside <think>...</think> tags. Only output visible reply text OUTSIDE of <think> blocks. Do NOT wrap your reply in any other tags like <final>. When you have tools available, prefer calling a tool over outputting text.".to_string(),
            "Context Awareness Protocol: Conversation history is segmented by recency markers. Always prioritize [CURRENT TASK] as the primary directive. If earlier history conflicts with [CURRENT TASK], follow [CURRENT TASK]. Use historical context only as background reference, not as active instructions.".to_string(),
            crate::security::system_security_prompt(),
        ])
    }

    pub fn new_subagent() -> Self {
        Self::with_system_prompts(vec![
            "You are a delegated sub-agent. Complete only the assigned goal with the available tools, then provide the final answer as plain text without calling another tool.".to_string(),
            "Delegated sub-agents must not ask the user questions directly. If blocked, report missing information or constraints in your final text response.".to_string(),
            "Use tools when needed, stay within the delegated scope, and be concise.".to_string(),
            crate::security::system_security_prompt(),
        ])
    }

    fn with_system_prompts(system_prompts: Vec<String>) -> Self {
        Self {
            system_prompts,
            dialogue_history: Vec::new(),
            current_turn: None,
            max_history_tokens: 1_000_000,
            transcript_path: None,
            retrieved_memory: None,
            retrieved_memory_sources: Vec::new(),
            last_snapshot: None,
            active_evidence: Vec::new(),
            skill_contract: None,
            skill_instructions: None,
            skill_state_summary: None,
            execution_notices: None,
        }
    }

    pub(crate) fn get_bpe() -> tiktoken_rs::CoreBPE {
        token::get_bpe()
    }

    pub fn with_transcript_path(mut self, transcript_path: PathBuf) -> Self {
        self.transcript_path = Some(transcript_path);
        self
    }

    pub fn get_detailed_stats(&self, pending_user_input: Option<&str>) -> DetailedContextStats {
        prompt::get_detailed_stats(self, pending_user_input)
    }

    pub fn take_snapshot(&mut self) -> ContextSnapshot {
        state::take_snapshot(self)
    }

    pub fn diff_snapshot(&self, old: &ContextSnapshot) -> ContextDiff {
        state::diff_snapshot(self, old)
    }

    pub fn load_transcript(&mut self) -> std::io::Result<usize> {
        transcript::load_into_context(self)
    }

    pub(crate) fn append_turn_to_transcript(&self, turn: &Turn) -> std::io::Result<()> {
        transcript::append_context_turn(self, turn)
    }

    pub(crate) fn estimate_tokens(bpe: &CoreBPE, msg: &Message) -> usize {
        token::estimate_tokens(bpe, msg)
    }

    pub(crate) fn truncate_chars(input: &str, max_chars: usize) -> String {
        token::truncate_chars(input, max_chars)
    }

    pub(crate) fn build_system_prompt(&self) -> String {
        prompt::build_system_prompt(self)
    }

    pub(crate) fn build_history_with_budget(&self) -> (Vec<Message>, usize, usize, usize) {
        super::history::build_history_with_budget(self)
    }

    pub(crate) fn build_history_with_token_budget(
        &self,
        history_budget: usize,
    ) -> (Vec<Message>, usize, usize, usize) {
        super::history::build_history_with_token_budget(self, history_budget)
    }

    pub(crate) fn turn_token_estimate(turn: &Turn, bpe: &CoreBPE) -> usize {
        token::turn_token_estimate(turn, bpe)
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

    pub fn rule_based_compact(&mut self, num_turns: usize) -> Option<String> {
        super::history::rule_based_compact(self, num_turns)
    }

    pub fn start_turn(&mut self, text: String) {
        turns::start_turn(self, text);
    }

    pub fn add_message_to_current_turn(&mut self, msg: Message) {
        turns::add_message_to_current_turn(self, msg);
    }

    pub fn compress_current_turn(&mut self, max_bytes: usize) -> usize {
        super::history::compress_current_turn(self, max_bytes)
    }

    pub fn truncate_current_turn_tool_results(&mut self, max_chars: usize) -> usize {
        super::history::truncate_current_turn_tool_results(self, max_chars)
    }

    pub fn end_turn(&mut self) {
        turns::end_turn(self);
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
        sanitize::strip_thinking_tags(text)
    }

    pub(crate) fn strip_response_payload(fr: &mut FunctionResponse) {
        sanitize::strip_response_payload(fr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::model::Part;
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
