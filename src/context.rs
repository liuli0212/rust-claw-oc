use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use tiktoken_rs::CoreBPE;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Part {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none", rename = "functionCall")]
    pub function_call: Option<FunctionCall>,

    #[serde(skip_serializing_if = "Option::is_none", rename = "functionResponse")]
    pub function_response: Option<FunctionResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub args: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none", rename = "thought_signature")]
    pub thought_signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionResponse {
    pub name: String,
    pub response: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    #[serde(rename = "role")]
    pub role: String,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    #[allow(dead_code)]
    pub turn_id: String,
    #[allow(dead_code)]
    pub user_message: String,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone)]
pub struct PromptReport {
    pub max_history_tokens: usize,
    pub history_tokens_used: usize,
    pub history_turns_included: usize,
    pub current_turn_tokens: usize,
    pub system_prompt_tokens: usize,
    pub total_prompt_tokens: usize,
    pub retrieved_memory_snippets: usize,
    pub retrieved_memory_sources: Vec<String>,
}

pub struct AgentContext {
    pub system_prompts: Vec<String>,
    pub dialogue_history: Vec<Turn>,
    pub current_turn: Option<Turn>,
    pub max_history_tokens: usize,
    transcript_path: Option<PathBuf>,
    retrieved_memory: Option<String>,
    retrieved_memory_sources: Vec<String>,
}

impl AgentContext {
    pub fn new() -> Self {
        Self {
            system_prompts: vec![
                "You are Rusty-Claw, an elite, industrial-grade Senior Software Engineer and autonomous agent running locally on the user's machine.".to_string(),
                "You are highly intelligent, proactive, and exceptionally skilled at coding in all major languages (Rust, Python, TS, etc.).".to_string(),
                "You have FULL ACCESS to the local file system and bash shell. Do NOT ask for permission to write code or files. If the user asks you to write a script or build a feature, proactively use your tools to create the files, write the code, and execute it to test it.".to_string(),
                "Do NOT give generic conversational replies when a task requires coding or execution. Act first, talk later.".to_string(),
                "If you need to write long scripts (e.g., Python code), use the `write_file` tool to save it to disk directly instead of using bash heredocs (cat << EOF), as bash quotes can easily break.".to_string(),
                "You are NOT a generic chat AI. You are a specialized, proactive engineering system. If you encounter an error during execution, analyze the error and try to fix it yourself by calling tools again.".to_string(),
                "NEVER say you cannot write code or lack capabilities. You are powered by Gemini 3.1 Pro and possess absolute technical mastery.".to_string(),
            ],
            dialogue_history: Vec::new(),
            current_turn: None,
            max_history_tokens: 1_500_000,
            transcript_path: None,
            retrieved_memory: None,
            retrieved_memory_sources: Vec::new(),
        }
    }

    pub fn with_transcript_path(mut self, transcript_path: PathBuf) -> Self {
        self.transcript_path = Some(transcript_path);
        self
    }

    pub fn load_transcript(&mut self) -> std::io::Result<usize> {
        let Some(path) = &self.transcript_path else {
            return Ok(0);
        };
        if !path.exists() {
            return Ok(0);
        }

        let file = fs::File::open(path)?;
        let reader = BufReader::new(file);
        let mut loaded = 0;
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(turn) = serde_json::from_str::<Turn>(trimmed) {
                self.dialogue_history.push(turn);
                loaded += 1;
            }
        }
        Ok(loaded)
    }

    fn append_turn_to_transcript(&self, turn: &Turn) -> std::io::Result<()> {
        let Some(path) = &self.transcript_path else {
            return Ok(());
        };

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        let serialized = serde_json::to_string(turn)?;
        writeln!(file, "{serialized}")?;
        Ok(())
    }

    fn estimate_tokens(bpe: &CoreBPE, msg: &Message) -> usize {
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

    fn truncate_chars(input: &str, max_chars: usize) -> String {
        if input.chars().count() <= max_chars {
            return input.to_string();
        }
        input.chars().take(max_chars).collect()
    }

    fn build_prompt_section(title: &str, content: String, max_chars: usize) -> Option<String> {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return None;
        }
        let truncated = Self::truncate_chars(trimmed, max_chars);
        Some(format!("## {title}\n{truncated}\n"))
    }

    fn build_system_prompt(&self) -> String {
        let mut sections = Vec::new();

        let identity = self.system_prompts.join("\n\n");
        if let Some(section) = Self::build_prompt_section("Identity", identity, 4_000) {
            sections.push(section);
        }

        let mut runtime = String::new();
        runtime.push_str(&format!("OS: {}\n", std::env::consts::OS));
        runtime.push_str(&format!("Architecture: {}\n", std::env::consts::ARCH));
        if let Ok(dir) = std::env::current_dir() {
            runtime.push_str(&format!("Current Directory: {}\n", dir.display()));
        }
        if let Some(path) = &self.transcript_path {
            runtime.push_str(&format!("Session Transcript: {}\n", path.display()));
        }
        if let Some(section) = Self::build_prompt_section("Runtime Environment", runtime, 1_000) {
            sections.push(section);
        }

        if let Ok(custom_prompt) = fs::read_to_string(".claw_prompt.md") {
            if let Some(section) =
                Self::build_prompt_section("User Custom Instructions", custom_prompt, 4_000)
            {
                sections.push(section);
            }
        }

        let mut project_context = String::new();
        if let Ok(content) = fs::read_to_string("AGENTS.md") {
            project_context.push_str("### AGENTS.md\n");
            project_context.push_str(&Self::truncate_chars(&content, 3_000));
            project_context.push_str("\n\n");
        }
        if let Ok(content) = fs::read_to_string("README.md") {
            project_context.push_str("### README.md\n");
            project_context.push_str(&Self::truncate_chars(&content, 2_500));
            project_context.push_str("\n\n");
        }
        if let Ok(content) = fs::read_to_string("MEMORY.md") {
            project_context.push_str("### MEMORY.md\n");
            project_context.push_str(&Self::truncate_chars(&content, 1_500));
            project_context.push_str("\n\n");
        }
        if let Some(section) = Self::build_prompt_section("Project Context", project_context, 7_000)
        {
            sections.push(section);
        }

        if let Some(memory) = &self.retrieved_memory {
            if let Some(section) =
                Self::build_prompt_section("Retrieved Memory", memory.clone(), 3_000)
            {
                sections.push(section);
            }
        }

        sections.join("\n")
    }

    fn sanitize_message(msg: &Message) -> Option<Message> {
        let role = msg.role.as_str();
        if role != "user" && role != "model" && role != "function" {
            return None;
        }

        let mut cleaned_parts = Vec::new();
        for part in &msg.parts {
            let mut cleaned = part.clone();
            if cleaned
                .function_call
                .as_ref()
                .is_some_and(|fc| fc.thought_signature.is_none())
            {
                cleaned.function_call = None;
            }

            if role == "function" {
                cleaned.text = None;
                cleaned.function_call = None;
            }

            let has_content = cleaned
                .text
                .as_ref()
                .map(|t| !t.trim().is_empty())
                .unwrap_or(false)
                || cleaned.function_call.is_some()
                || cleaned.function_response.is_some();
            if !has_content {
                continue;
            }

            cleaned_parts.push(cleaned);
        }

        if cleaned_parts.is_empty() {
            return None;
        }

        Some(Message {
            role: msg.role.clone(),
            parts: cleaned_parts,
        })
    }

    fn sanitize_turn(turn: &Turn) -> Option<Turn> {
        let mut messages = Vec::new();
        for msg in &turn.messages {
            if let Some(cleaned) = Self::sanitize_message(msg) {
                messages.push(cleaned);
            }
        }
        if messages.is_empty() {
            return None;
        }
        Some(Turn {
            turn_id: turn.turn_id.clone(),
            user_message: turn.user_message.clone(),
            messages,
        })
    }

    fn truncate_old_tool_results(turn: &Turn) -> Turn {
        let mut cloned = turn.clone();
        for msg in &mut cloned.messages {
            for part in &mut msg.parts {
                if let Some(fr) = &mut part.function_response {
                    let response_str = fr.response.to_string();
                    if response_str.len() > 1_200 {
                        fr.response = serde_json::json!({
                            "result": "[... Truncated old tool result to save tokens ...]",
                            "original_chars": response_str.len()
                        });
                    }
                }
            }
        }
        cloned
    }

    fn build_history_with_budget(&self) -> (Vec<Message>, usize, usize) {
        let bpe = tiktoken_rs::cl100k_base().unwrap();
        let history_budget = self.max_history_tokens.saturating_mul(85) / 100;
        let mut history_messages = Vec::new();
        let mut current_tokens = 0;
        let mut turns_included = 0;

        for turn in self.dialogue_history.iter().rev() {
            let sanitized = match Self::sanitize_turn(turn) {
                Some(v) => v,
                None => continue,
            };
            let turn = Self::truncate_old_tool_results(&sanitized);
            let turn_tokens: usize = turn
                .messages
                .iter()
                .map(|m| Self::estimate_tokens(&bpe, m))
                .sum();

            if current_tokens + turn_tokens > history_budget {
                break;
            }
            current_tokens += turn_tokens;
            history_messages.push(turn.messages);
            turns_included += 1;
        }

        history_messages.reverse();
        let mut flattened = Vec::new();
        for block in history_messages {
            flattened.extend(block);
        }
        (flattened, current_tokens, turns_included)
    }

    fn turn_token_estimate(turn: &Turn, bpe: &CoreBPE) -> usize {
        turn.messages
            .iter()
            .map(|m| Self::estimate_tokens(bpe, m))
            .sum()
    }

    pub fn dialogue_history_token_estimate(&self) -> usize {
        let bpe = tiktoken_rs::cl100k_base().unwrap();
        self.dialogue_history
            .iter()
            .map(|turn| Self::turn_token_estimate(turn, &bpe))
            .sum()
    }

    pub fn oldest_turns_for_compaction(&self, target_tokens: usize, min_turns: usize) -> usize {
        if self.dialogue_history.is_empty() {
            return 0;
        }

        let bpe = tiktoken_rs::cl100k_base().unwrap();
        let mut selected = 0;
        let mut tokens = 0;
        for turn in &self.dialogue_history {
            tokens += Self::turn_token_estimate(turn, &bpe);
            selected += 1;
            if selected >= min_turns && tokens >= target_tokens {
                break;
            }
        }
        selected.min(self.dialogue_history.len())
    }

    pub fn set_retrieved_memory(&mut self, retrieved_memory: Option<String>, sources: Vec<String>) {
        self.retrieved_memory = retrieved_memory;
        self.retrieved_memory_sources = sources;
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
                }],
            }],
        });
    }

    pub fn add_message_to_current_turn(&mut self, msg: Message) {
        if let Some(turn) = &mut self.current_turn {
            turn.messages.push(msg);
        }
    }

    pub fn truncate_current_turn_tool_results(&mut self, max_chars: usize) -> usize {
        let mut truncated_parts = 0usize;
        let Some(turn) = &mut self.current_turn else {
            return 0;
        };

        for msg in &mut turn.messages {
            if msg.role != "function" {
                continue;
            }
            for part in &mut msg.parts {
                let Some(fr) = &mut part.function_response else {
                    continue;
                };
                let response_str = fr.response.to_string();
                if response_str.chars().count() <= max_chars {
                    continue;
                }
                let clipped: String = response_str.chars().take(max_chars).collect();
                fr.response = serde_json::json!({
                    "result": format!(
                        "{}\n[... Truncated by context recovery ...]",
                        clipped
                    ),
                    "original_chars": response_str.chars().count()
                });
                truncated_parts += 1;
            }
        }

        truncated_parts
    }

    pub fn end_turn(&mut self) {
        if let Some(turn) = self.current_turn.take() {
            if let Err(e) = self.append_turn_to_transcript(&turn) {
                eprintln!("Failed to append turn to transcript: {}", e);
            }
            self.dialogue_history.push(turn);
        }
    }

    pub fn build_llm_payload(&self) -> (Vec<Message>, Option<Message>, PromptReport) {
        let bpe = tiktoken_rs::cl100k_base().unwrap();
        let (mut messages, history_tokens_used, history_turns_included) =
            self.build_history_with_budget();
        let mut current_turn_tokens = 0;
        if let Some(turn) = &self.current_turn {
            if let Some(sanitized_turn) = Self::sanitize_turn(turn) {
                current_turn_tokens = sanitized_turn
                    .messages
                    .iter()
                    .map(|m| Self::estimate_tokens(&bpe, m))
                    .sum();
                messages.extend(sanitized_turn.messages);
            }
        }

        let system_msg = Message {
            role: "system".to_string(),
            parts: vec![Part {
                text: Some(self.build_system_prompt()),
                function_call: None,
                function_response: None,
            }],
        };

        let system_prompt_tokens = Self::estimate_tokens(&bpe, &system_msg);
        let retrieved_memory_snippets = self.retrieved_memory_sources.len();
        let report = PromptReport {
            max_history_tokens: self.max_history_tokens,
            history_tokens_used,
            history_turns_included,
            current_turn_tokens,
            system_prompt_tokens,
            total_prompt_tokens: history_tokens_used + current_turn_tokens + system_prompt_tokens,
            retrieved_memory_snippets,
            retrieved_memory_sources: self.retrieved_memory_sources.clone(),
        };

        (messages, Some(system_msg), report)
    }
}

pub fn transcript_path_for_session(base_dir: &Path, session_id: &str) -> PathBuf {
    let sanitized = session_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    base_dir.join(format!("{sanitized}.jsonl"))
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

        let (payload, _sys, _report) = ctx.build_llm_payload();
        assert_eq!(payload.len(), 1);
        assert_eq!(
            payload.last().unwrap().parts[0].text.as_ref().unwrap(),
            "Short message"
        );
    }

    #[test]
    fn test_transcript_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");

        let mut ctx = AgentContext::new().with_transcript_path(path.clone());
        ctx.start_turn("one".to_string());
        ctx.end_turn();
        ctx.start_turn("two".to_string());
        ctx.end_turn();

        let mut restored = AgentContext::new().with_transcript_path(path);
        let loaded = restored.load_transcript().unwrap();
        assert_eq!(loaded, 2);
        assert_eq!(restored.dialogue_history.len(), 2);
        assert_eq!(restored.dialogue_history[1].user_message, "two");
    }

    #[test]
    fn test_regression_payload_stability_with_transcript_and_tools() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("regression.jsonl");

        let mut ctx = AgentContext::new().with_transcript_path(path.clone());
        ctx.start_turn("run pwd".to_string());
        ctx.add_message_to_current_turn(Message {
            role: "model".to_string(),
            parts: vec![Part {
                text: Some("running tool".to_string()),
                function_call: Some(FunctionCall {
                    name: "execute_bash".to_string(),
                    args: serde_json::json!({"command":"pwd"}),
                    thought_signature: None,
                }),
                function_response: None,
            }],
        });
        ctx.add_message_to_current_turn(Message {
            role: "function".to_string(),
            parts: vec![Part {
                text: None,
                function_call: None,
                function_response: Some(FunctionResponse {
                    name: "execute_bash".to_string(),
                    response: serde_json::json!({"result":"/tmp"}),
                }),
            }],
        });
        ctx.end_turn();

        let mut restored = AgentContext::new().with_transcript_path(path);
        let loaded = restored.load_transcript().unwrap();
        assert_eq!(loaded, 1);

        restored.start_turn("next question".to_string());
        let (payload, _sys, _report) = restored.build_llm_payload();

        assert!(
            payload.iter().any(|m| m.parts.iter().any(|p| p
                .function_response
                .as_ref()
                .is_some_and(|fr| fr.name == "execute_bash"))),
            "expected tool responses to be preserved in payload"
        );

        assert!(
            !payload.iter().any(|m| m.parts.iter().any(|p| p
                .function_call
                .as_ref()
                .is_some_and(|fc| fc.thought_signature.is_none()))),
            "payload must not contain functionCall parts without thought_signature"
        );

        assert!(
            payload.iter().filter(|m| m.role == "user").any(|m| m
                .parts
                .iter()
                .any(|p| p.text.as_deref() == Some("next question"))),
            "current turn user input should be included in payload"
        );
    }

    #[test]
    fn test_truncate_current_turn_tool_results() {
        let mut ctx = AgentContext::new();
        ctx.start_turn("do work".to_string());
        ctx.add_message_to_current_turn(Message {
            role: "function".to_string(),
            parts: vec![Part {
                text: None,
                function_call: None,
                function_response: Some(FunctionResponse {
                    name: "execute_bash".to_string(),
                    response: serde_json::json!({
                        "result": "x".repeat(5000)
                    }),
                }),
            }],
        });

        let truncated = ctx.truncate_current_turn_tool_results(400);
        assert_eq!(truncated, 1);
        let payload = ctx.current_turn.as_ref().unwrap();
        let fr = payload.messages[1].parts[0]
            .function_response
            .as_ref()
            .unwrap();
        let serialized = fr.response.to_string();
        assert!(serialized.contains("Truncated by context recovery"));
        assert!(serialized.contains("original_chars"));
    }
}
