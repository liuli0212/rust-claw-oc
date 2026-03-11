#![allow(warnings)]
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
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

    #[serde(skip_serializing_if = "Option::is_none", rename = "thoughtSignature")]
    pub thought_signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub args: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionResponse {
    pub name: String,
    pub response: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none", alias = "tool_call_id")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    #[serde(rename = "role")]
    pub role: String,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub turn_id: String,
    pub user_message: String,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DetailedContextStats {
    pub system_static: usize,
    pub system_runtime: usize,
    pub system_custom: usize,  // .claw_prompt.md
    pub system_project: usize, // AGENTS.md, etc.
    pub system_task_plan: usize,
    pub memory: usize,
    pub history: usize,
    pub current_turn: usize,
    pub last_turn: usize,
    pub total: usize,
    pub max: usize,
    pub truncated_chars: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSnapshot {
    pub timestamp: u64,
    pub turn_id: String,
    pub stats: DetailedContextStats,
    pub messages_count: usize,
    pub system_prompt_hash: u64,
    pub retrieved_memory_sources: Vec<String>,
    pub history_turns_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextDiff {
    pub token_delta: i64,
    pub history_turns_delta: i32,
    pub system_prompt_changed: bool,
    pub new_sources: Vec<String>,
    pub removed_sources: Vec<String>,
    pub memory_changed: bool,
    pub truncated_delta: i64,
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
    pub detailed_stats: DetailedContextStats,
}

pub struct AgentContext {
    pub system_prompts: Vec<String>,
    pub dialogue_history: Vec<Turn>,
    pub current_turn: Option<Turn>,
    pub max_history_tokens: usize,
    transcript_path: Option<PathBuf>,
    retrieved_memory: Option<String>,
    retrieved_memory_sources: Vec<String>,
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
                "You are NOT a generic chat AI. You are a specialized, proactive engineering system. If you encounter an error during execution, analyze the error and try to fix it yourself by calling tools again.".to_string(),
                "CRITICAL: Unless the user explicitly asks you to 'plan', 'design', 'investigate', or 'analyze' without acting, you MUST take immediate action by returning a valid JSON tool call (e.g., `read_file`, `execute_bash`).".to_string(),
                "CRITICAL: Do NOT ask for permission to continue. If your task requires multiple steps (like reading several files sequentially), you MUST call tools sequentially until the task is completely finished. Do not stop and output conversational text to explain your next steps. Act, do not chat.".to_string(),
                "CRITICAL: Be thorough and execute completely. Do not take lazy shortcuts. If a task requires inspecting multiple files, you MUST use `read_file` on them. Do not guess or hallucinate contents based on file names. Do not call `finish_task` until all implicit requirements are truly met and verified.".to_string(),
                "NEVER say you cannot write code or lack capabilities. You possess absolute technical mastery.".to_string(),
                "VERY VERY CRITICAL: When you have fully completed the user's request and there is absolutely nothing left to do, you MUST call the `finish_task` tool. Otherwise you will be in DEAD LOOP, NEVER exit.".to_string(),
                "ALL internal reasoning MUST be inside <think>...</think> tags. Only output visible reply text OUTSIDE of <think> blocks. Do NOT wrap your reply in any other tags like <final>. When you have tools available, prefer calling a tool over outputting text.".to_string(),
                "Context Awareness Protocol: Conversation history is segmented by recency markers. --- [EARLIER HISTORY] --- contains old context, --- [RECENT CONTEXT] --- contains moderately recent turns, unmarked turns are the most recent history, and --- [CURRENT TASK] --- marks the active user instruction. Always prioritize [CURRENT TASK] as the primary directive. If earlier history conflicts with [CURRENT TASK], follow [CURRENT TASK]. Use historical context only as background reference, not as active instructions.".to_string(),
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

    pub fn get_bpe() -> CoreBPE {
        tiktoken_rs::cl100k_base().unwrap()
    }

    pub fn with_transcript_path(mut self, transcript_path: PathBuf) -> Self {
        self.transcript_path = Some(transcript_path);
        self
    }

    /// Single source of truth for system prompt sections.
    /// Returns (assembled_prompt, per_section_token_counts).
    fn build_prompt_sections(&self) -> (String, DetailedContextStats) {
        let bpe = Self::get_bpe();
        let mut stats = DetailedContextStats::default();
        let mut sections = Vec::new();

        // 1. Identity
        let identity = self.system_prompts.join("\n\n");
        if let Some(section) = Self::build_prompt_section("Identity", identity.clone(), 4_000) {
            stats.system_static = bpe.encode_with_special_tokens(&section).len();
            sections.push(section);
        }

        // 2. Runtime
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
            stats.system_runtime = bpe.encode_with_special_tokens(&section).len();
            sections.push(section);
        }

        // 3. Custom Instructions
        if let Ok(custom_prompt) = fs::read_to_string(".claw_prompt.md") {
            if let Some(section) =
                Self::build_prompt_section("User Custom Instructions", custom_prompt, 4_000)
            {
                stats.system_custom = bpe.encode_with_special_tokens(&section).len();
                sections.push(section);
            }
        }

        // 4. Task Plan
        // Deprecated: the task plan is now loaded via context assembler dynamically from events.
        // We leave the block stubbed here for structural layout compat in build_prompt_sections if any other code relied on length.
        // 5. Project Context
        // [P1-1.4 Fix] Task Planning instruction only injected when NO active plan exists
        let mut project_context = String::new();
        if stats.system_task_plan == 0 {
            project_context.push_str("### Task Planning\n");
            project_context.push_str("If the user request is complex (e.g. multi-step refactoring, new feature implementation), you MUST use the `task_plan` tool immediately to create a structured plan (action='add').\n\n");
        }
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
            stats.system_project = bpe.encode_with_special_tokens(&section).len();
            sections.push(section);
        }

        // 6. Retrieved Memory (RAG)
        if let Some(memory) = &self.retrieved_memory {
            if let Some(section) =
                Self::build_prompt_section("Retrieved Memory", memory.clone(), 3_000)
            {
                stats.memory = bpe.encode_with_special_tokens(&section).len();
                sections.push(section);
            }
        }

        stats.max = self.max_history_tokens;
        (sections.join("\n"), stats)
    }

    pub fn get_detailed_stats(&self, pending_user_input: Option<&str>) -> DetailedContextStats {
        let (_, mut stats) = self.build_prompt_sections();
        let bpe = Self::get_bpe();

        // History (Net Load)
        let (_, history_tokens, _, truncated_chars) = self.build_history_with_budget();
        stats.history = history_tokens;
        stats.truncated_chars = truncated_chars;

        // Current Turn
        if let Some(turn) = &self.current_turn {
            for msg in &turn.messages {
                for part in &msg.parts {
                    if let Some(text) = &part.text {
                        stats.current_turn += bpe.encode_with_special_tokens(text).len();
                    }
                    if let Some(fc) = &part.function_call {
                        stats.current_turn += bpe.encode_with_special_tokens(&fc.name).len();
                        stats.current_turn +=
                            bpe.encode_with_special_tokens(&fc.args.to_string()).len();
                    }
                    if let Some(fr) = &part.function_response {
                        stats.current_turn += bpe.encode_with_special_tokens(&fr.name).len();
                        stats.current_turn += bpe
                            .encode_with_special_tokens(&fr.response.to_string())
                            .len();
                    }
                }
            }
        } else if let Some(input) = pending_user_input {
            stats.current_turn = bpe.encode_with_special_tokens(input).len();
        }

        // Last Turn
        if let Some(last) = self.dialogue_history.last() {
            stats.last_turn = Self::turn_token_estimate(last, &bpe);
        }

        // Total
        stats.total = stats.system_static
            + stats.system_runtime
            + stats.system_custom
            + stats.system_project
            + stats.system_task_plan
            + stats.memory
            + stats.history
            + stats.current_turn;

        stats
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
        let (prompt, _) = self.build_prompt_sections();
        prompt
    }

    fn sanitize_message(msg: &Message) -> Option<Message> {
        let role = msg.role.as_str();
        if role != "user" && role != "model" && role != "function" {
            return None;
        }

        let mut cleaned_parts = Vec::new();
        for part in &msg.parts {
            let mut cleaned = part.clone();

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

    fn truncate_old_tool_results(turn: &Turn) -> (Turn, usize) {
        let mut cloned = turn.clone();
        for msg in &mut cloned.messages {
            for part in &mut msg.parts {
                if let Some(fr) = &mut part.function_response {
                    // Smart truncation: Try to truncate the "result" field inside the JSON first
                    let mut truncated_in_place = false;

                    // We need to work with the value as a mutable object if possible
                    if let Some(obj) = fr.response.as_object_mut() {
                        if let Some(val) = obj.get_mut("result") {
                            // Case 1: result is a String (common for read_file, execute_bash)
                            if let Some(s) = val.as_str() {
                                // TIERED CONTEXT STRATEGY:
                                // History items are compressed to 12,000 chars (Head 6k + Tail 6k).
                                // This retains context & errors while saving tokens.
                                if s.len() > 12_000 {
                                    let char_count = s.chars().count();
                                    if char_count > 12_000 {
                                        let keep = 6_000;
                                        let head: String = s.chars().take(keep).collect();
                                        let tail: String =
                                            s.chars().skip(char_count - keep).collect();

                                        *val = serde_json::Value::String(format!(
                                            "{}\n... [History Compressed: {} chars hidden] ...\n{}",
                                            head,
                                            char_count - 12_000,
                                            tail
                                        ));
                                        truncated_in_place = true;
                                    }
                                }
                            }
                            // Case 2: result is a large object
                            else if val.to_string().len() > 12_000 {
                                let s = val.to_string();
                                let char_count = s.chars().count();
                                if char_count > 12_000 {
                                    let head: String = s.chars().take(4_000).collect();
                                    *val = serde_json::Value::String(format!(
                                        "{}\n... [History Object Compressed] ...",
                                        head
                                    ));
                                    truncated_in_place = true;
                                }
                            }
                        }
                    }

                    // Fallback safety cap
                    if !truncated_in_place {
                        let response_str = fr.response.to_string();
                        if response_str.len() > 20_000 {
                            let head: String = response_str.chars().take(2_000).collect();
                            fr.response = serde_json::json!({
                                "result": format!("{}\n... [Truncated massive object] ...", head),
                                "original_chars": response_str.len()
                            });
                        }
                    }
                }
            }
        }
        (cloned, 0)
    }

    fn is_user_referencing_history(msg: &str) -> bool {
        let lower = msg.to_lowercase();
        let keywords = [
            // English
            "previous command",
            "last command",
            "previous output",
            "last output",
            "what did it say",
            "fix the error",
            "look above",
            "check the error",
            "what was the error",
            "show me the output",
            "full output",
            // Chinese (P1-1.5 fix)
            "上次",
            "之前",
            "刚才",
            "前面",
            "上面",
            "修复错误",
            "看看输出",
            "检查错误",
            "什么错误",
            "历史",
            "回顾",
            "重新看",
        ];
        keywords.iter().any(|k| lower.contains(k))
    }

    fn strip_thinking_tags(text: &str) -> String {
        let mut result = text.to_string();
        while let Some(start) = result.find("<think>") {
            if let Some(end) = result.find("</think>") {
                let before = &result[..start];
                let after = &result[end + 8..]; // 8 is len of </think>
                result = format!("{}{}", before, after);
            } else {
                // If there's an unclosed <think> tag, just strip from <think> to the end of the string
                // and break, otherwise we have an infinite loop!
                result = result[..start].to_string();
                break;
            }
        }
        result.trim().to_string()
    }

    fn strip_response_payload(fr: &mut FunctionResponse) {
        // All tool results from core.rs are wrapped as: { "result": "{...serialized ToolExecutionEnvelope...}" }
        // We unwrap the envelope, strip per-tool, remove noise fields, and re-serialize.
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

        // Try to parse the inner envelope JSON
        let mut envelope: serde_json::Value = match serde_json::from_str(&result_str) {
            Ok(v) => v,
            Err(_) => {
                // Not valid JSON — just truncate the raw string
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
                // Plan is always in system prompt; replace entire result
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
            "write_file" | "patch_file" => {
                // Already small, keep as-is
            }
            _ => {
                // Generic: truncate output if large
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

        // Strip noise fields from envelope to save tokens
        env_obj.remove("duration_ms");
        env_obj.remove("truncated");
        env_obj.remove("recovery_attempted");
        env_obj.remove("recovery_output");
        env_obj.remove("recovery_rule");

        // Re-serialize the stripped envelope back
        if let Ok(stripped_str) = serde_json::to_string(env_obj) {
            *result_val = serde_json::Value::String(stripped_str);
        }
    }

    fn reconstruct_turn_for_history(turn: &Turn) -> (Turn, usize) {
        let mut new_messages = Vec::new();

        println!("      [Reconstruct] processing {} messages", turn.messages.len());
        for (m_idx, msg) in turn.messages.iter().enumerate() {
            println!("      [Reconstruct msg {}] {} parts", m_idx, msg.parts.len());
            let mut new_parts = Vec::new();

            for (p_idx, part) in msg.parts.iter().enumerate() {
                println!("        [Reconstruct part {}]", p_idx);
                let mut new_part = Part {
                    text: None,
                    function_call: None,
                    function_response: None,
                    thought_signature: None, // Strip for history — Gemini only validates current turn
                };

                // 1. Function Call (Action) - KEEP (but strip task_plan args)
                if let Some(fc) = &part.function_call {
                    println!("          - function_call: {}", fc.name);
                    if fc.name == "task_plan" {
                        // For task_plan, only keep the action to save tokens
                        let action = fc
                            .args
                            .get("action")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        let mut stripped_fc = fc.clone();
                        stripped_fc.args = serde_json::json!({ "action": action });
                        new_part.function_call = Some(stripped_fc);
                    } else {
                        new_part.function_call = Some(fc.clone());
                    }
                }

                // 2. Function Response (Result) - KEEP (Stripped)
                if let Some(fr) = &part.function_response {
                    println!("          - function_response: {}", fr.name);
                    let mut stripped_fr = fr.clone();
                    Self::strip_response_payload(&mut stripped_fr);
                    new_part.function_response = Some(stripped_fr);
                }

                // 3. Text (Intent/Reply) - SELECTIVE KEEP
                if let Some(text) = &part.text {
                    println!("          - text (len: {})", text.len());
                    if msg.role == "user" {
                        // User text: Keep, but clean system tags
                        let mut cleaned_text = text.clone();
                        let markers = [
                            "[CURRENT TASK]",
                            "--- [RECENT CONTEXT] ---",
                            "--- [EARLIER HISTORY] ---",
                        ];
                        for marker in markers {
                            if cleaned_text.contains(marker) {
                                cleaned_text = cleaned_text.replace(marker, "").trim().to_string();
                            }
                        }
                        new_part.text = Some(cleaned_text);
                    } else if msg.role == "model" {
                        // Model text: Only keep if NO function call, and strip <think>
                        if new_part.function_call.is_none() {
                            println!("          - stripping think tags for model text...");
                            let cleaned = Self::strip_thinking_tags(text);
                            println!("          - think stripping done.");
                            if !cleaned.is_empty() {
                                new_part.text = Some(cleaned);
                            }
                        }
                    }
                }

                // Only add part if it has content
                if new_part.text.is_some()
                    || new_part.function_call.is_some()
                    || new_part.function_response.is_some()
                {
                    new_parts.push(new_part);
                }
            }

            if !new_parts.is_empty() {
                new_messages.push(Message {
                    role: msg.role.clone(),
                    parts: new_parts,
                });
            }
        }

        (
            Turn {
                turn_id: turn.turn_id.clone(),
                user_message: turn.user_message.clone(),
                messages: new_messages,
            },
            0,
        )
    }

    fn build_history_with_budget(&self) -> (Vec<Message>, usize, usize, usize) {
        let bpe = tiktoken_rs::cl100k_base().unwrap();
        let history_budget = self.max_history_tokens.saturating_mul(85) / 100;
        // Each entry: (distance_index, messages)
        let mut history_blocks: Vec<(usize, Vec<Message>)> = Vec::new();
        let mut current_tokens = 0;
        let mut turns_included = 0;
        let mut total_truncated_chars = 0;

        let mut protect_next_turn = false;

        for (i, turn) in self.dialogue_history.iter().rev().enumerate() {
            let sanitized = match Self::sanitize_turn(turn) {
                Some(v) => v,
                None => continue,
            };

            // Heuristic: If this turn asks about history, protect the *next* turn we process (which is the older one)
            let user_asks_for_context = Self::is_user_referencing_history(&turn.user_message);

            // Context Optimization:
            // 1. Safety Buffer (Hot State): Keep last 3 turns in Full Fidelity.
            // 2. Heuristic Protection: If user asked "what did it say?", keep the older turn full.
            // 3. History (Cool State): Otherwise, apply Smart Stripping.
            let should_strip = i >= 3 && !protect_next_turn;

            let (turn, truncated) = if should_strip {
                Self::reconstruct_turn_for_history(&sanitized)
            } else {
                Self::truncate_old_tool_results(&sanitized)
            };
            total_truncated_chars += truncated;

            protect_next_turn = user_asks_for_context;

            let turn_tokens: usize = turn
                .messages
                .iter()
                .map(|m| Self::estimate_tokens(&bpe, m))
                .sum();

            if current_tokens + turn_tokens > history_budget {
                break;
            }
            current_tokens += turn_tokens;
            history_blocks.push((i, turn.messages));
            turns_included += 1;
        }

        history_blocks.reverse();

        // Inject zone separator labels based on turn distance.
        // Hot Zone (distance < 3): no label
        // Warm Zone (3 <= distance < 10): "--- [RECENT CONTEXT] ---"
        // Cold Zone (distance >= 10): "--- [EARLIER HISTORY] ---"
        let mut flattened = Vec::new();
        let mut prev_zone: Option<u8> = None; // 0=cold, 1=warm, 2=hot

        for (distance, block) in &history_blocks {
            let zone = if *distance >= 10 {
                0u8 // cold
            } else if *distance >= 3 {
                1u8 // warm
            } else {
                2u8 // hot
            };

            // Insert a zone separator when transitioning between zones
            if prev_zone.is_none() || prev_zone != Some(zone) {
                let label = match zone {
                    0 => Some("--- [EARLIER HISTORY] ---"),
                    1 => Some("--- [RECENT CONTEXT] ---"),
                    _ => None, // Hot zone: no label
                };
                if let Some(label_text) = label {
                    flattened.push(Message {
                        role: "user".to_string(),
                        parts: vec![Part {
                            text: Some(label_text.to_string()),
                            function_call: None,
                            function_response: None,
                            thought_signature: None,
                        }],
                    });
                }
            }
            prev_zone = Some(zone);
            flattened.extend(block.clone());
        }

        (
            flattened,
            current_tokens,
            turns_included,
            total_truncated_chars,
        )
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

    // Refactored to return accurate NET tokens (using compression)
    pub fn get_context_status(&self) -> (usize, usize, usize, usize, usize) {
        let bpe = tiktoken_rs::cl100k_base().unwrap();

        // 1. Calculate History (Net - Compressed)
        let (_, history_tokens, _, _) = self.build_history_with_budget();

        // 2. Current Turn
        let current_turn_tokens = if let Some(turn) = &self.current_turn {
            Self::turn_token_estimate(turn, &bpe)
        } else if let Some(last) = self.dialogue_history.last() {
            // For status display when idle, show last turn
            Self::turn_token_estimate(last, &bpe)
        } else {
            0
        };

        // 3. System Prompt (Net - Truncated)
        let system_msg = Message {
            role: "system".to_string(),
            parts: vec![Part {
                thought_signature: None,
                text: Some(self.build_system_prompt()),
                function_call: None,
                function_response: None,
            }],
        };
        let system_tokens = Self::estimate_tokens(&bpe, &system_msg);

        // 4. Accurate Total
        let total_tokens = history_tokens + current_turn_tokens + system_tokens;

        (
            total_tokens,
            self.max_history_tokens,
            history_tokens,
            current_turn_tokens,
            system_tokens,
        )
    }

    pub fn get_context_details(&self) -> String {
        let _bpe = Self::get_bpe();
        let stats = self.get_detailed_stats(None);

        let mut details = String::new();
        details.push_str("\n\x1b[1;36m=== Context Audit Report ===\x1b[0m\n");

        details.push_str(&format!(
            "\x1b[1;33m[Token Budget]\x1b[0m  {}/{} tokens ({:.1}% used)\n",
            stats.total,
            stats.max,
            (stats.total as f64 / stats.max as f64) * 100.0
        ));

        details.push_str("\n\x1b[1;33m[System Components]\x1b[0m\n");
        details.push_str(&format!(
            "  - Identity (Static):   {} tokens\n",
            stats.system_static
        ));
        details.push_str(&format!(
            "  - Runtime Env:        {} tokens\n",
            stats.system_runtime
        ));

        if stats.system_custom > 0 {
            details.push_str(&format!(
                "  - Custom Prompt:      {} tokens (.claw_prompt.md)\n",
                stats.system_custom
            ));
        }

        if stats.system_task_plan > 0 {
            details.push_str(&format!(
                "  - Task Plan:          {} tokens\n",
                stats.system_task_plan
            ));
        }

        details.push_str(&format!(
            "  - Project Context:    {} tokens\n",
            stats.system_project
        ));
        let project_files = ["AGENTS.md", "README.md", "MEMORY.md"];
        for file in project_files {
            if let Ok(meta) = fs::metadata(file) {
                details.push_str(&format!("    * {} ({} bytes)\n", file, meta.len()));
            }
        }

        details.push_str("\n\x1b[1;33m[Conversation History]\x1b[0m\n");
        let (_, _, turns_included, _) = self.build_history_with_budget();
        details.push_str(&format!(
            "  - History Load:       {} tokens ({} turns included)\n",
            stats.history, turns_included
        ));
        details.push_str(&format!(
            "  - Total History:      {} tokens ({} turns total)\n",
            self.dialogue_history_token_estimate(),
            self.dialogue_history.len()
        ));

        if stats.memory > 0 {
            details.push_str("\n\x1b[1;33m[RAG Memory]\x1b[0m\n");
            details.push_str(&format!(
                "  - Retrieved:          {} tokens\n",
                stats.memory
            ));
            for src in &self.retrieved_memory_sources {
                details.push_str(&format!("    * {}\n", src));
            }
        }

        if let Some(turn) = &self.current_turn {
            details.push_str("\n\x1b[1;33m[Current Turn]\x1b[0m\n");
            details.push_str(&format!(
                "  - Active Payload:     {} tokens\n",
                stats.current_turn
            ));
            details.push_str(&format!(
                "  - User Message:       {}\n",
                Self::truncate_chars(&turn.user_message, 80)
            ));
        }

        details
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

    /// Rule-based compaction: compress oldest N turns into a single structured summary Turn.
    /// No LLM call required. Preserves: user intent, tool names, key args, success/fail, errors.
    pub fn rule_based_compact(&mut self, num_turns: usize) -> Option<String> {
        if num_turns == 0 || self.dialogue_history.is_empty() {
            return None;
        }
        let to_compact = num_turns.min(self.dialogue_history.len());

        // [Risk 2] Archive turns to transcript before destructive drain
        for turn in self.dialogue_history.iter().take(to_compact) {
            if let Err(e) = self.append_turn_to_transcript(turn) {
                tracing::warn!("Failed to archive turn before compaction: {}", e);
            }
        }

        // Drain the oldest turns
        let compacted_turns: Vec<Turn> = self.dialogue_history.drain(0..to_compact).collect();

        // [Risk 3] Summary size cap
        const MAX_SUMMARY_CHARS: usize = 4000;

        // Build structured summary
        let mut summary_lines = Vec::new();
        let mut total_chars = 0;
        let mut budget_exhausted = false;

        summary_lines.push(format!("=== Compacted History ({} turns) ===", to_compact));
        total_chars += summary_lines[0].len();

        for (i, turn) in compacted_turns.iter().enumerate() {
            if budget_exhausted {
                summary_lines.push(format!(
                    "\n[Turns {}-{}] (omitted due to summary size limit)",
                    i + 1,
                    to_compact
                ));
                break;
            }

            // [Risk 1] Detect already-compacted turns — merge directly, don't re-extract
            if turn.user_message == "[SYSTEM] History Compacted" {
                for msg in &turn.messages {
                    for part in &msg.parts {
                        if let Some(text) = &part.text {
                            summary_lines.push(format!("\n{}", Self::truncate_chars(text, 1500)));
                            total_chars += text.len().min(1500);
                        }
                    }
                }
                continue;
            }

            let header = format!(
                "\n[Turn {}] User: {}",
                i + 1,
                Self::truncate_chars(&turn.user_message, 120)
            );
            total_chars += header.len();
            summary_lines.push(header);

            let mut actions = Vec::new();

            for msg in &turn.messages {
                for part in &msg.parts {
                    // Extract model's conclusion/reply (strip <think> blocks, keep first 200 chars)
                    if msg.role == "model" {
                        if let Some(text) = &part.text {
                            let cleaned = Self::strip_thinking_tags(text);
                            if !cleaned.is_empty() {
                                let preview = Self::truncate_chars(&cleaned, 200);
                                actions.push(format!("  💬 Agent: {}", preview));
                            }
                        }
                    }
                    // Extract tool calls
                    if let Some(fc) = &part.function_call {
                        let args_summary = Self::summarize_tool_args(&fc.name, &fc.args);
                        actions.push(format!("  → {}({})", fc.name, args_summary));
                    }
                    // [Risk 5] Extract tool results with improved error detection
                    if let Some(fr) = &part.function_response {
                        let is_error = Self::detect_tool_error(&fr.response);

                        if is_error {
                            let result_str = fr.response.to_string();
                            let error_preview = Self::truncate_chars(&result_str, 150);
                            actions.push(format!("  ✗ {} FAILED: {}", fr.name, error_preview));
                        } else {
                            actions.push(format!("  ✓ {} OK", fr.name));
                        }
                    }
                }
            }

            if actions.is_empty() {
                summary_lines.push("  (text-only exchange, no tool calls)".to_string());
            } else {
                for action in &actions {
                    total_chars += action.len();
                }
                summary_lines.extend(actions);
            }

            // [Risk 3] Check budget after each turn
            if total_chars > MAX_SUMMARY_CHARS {
                budget_exhausted = true;
            }
        }

        // [Risk 6] Add system guidance prefix so LLM treats this as context, not a user request
        let summary_text = format!(
            "[System context: The following is an automated summary of earlier conversation history. Use it as background knowledge but do not respond to it directly.]\n\n{}",
            summary_lines.join("\n")
        );

        // Create a single compacted Turn
        let compacted_turn = Turn {
            turn_id: format!("compacted-{}", uuid::Uuid::new_v4()),
            user_message: "[SYSTEM] History Compacted".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                parts: vec![Part {
                    text: Some(summary_text),
                    function_call: None,
                    function_response: None,
                    thought_signature: None,
                }],
            }],
        };

        // Insert at beginning
        self.dialogue_history.insert(0, compacted_turn);

        let reason = format!("Compacted {} turns into structured summary", to_compact);
        tracing::info!("{}", reason);
        Some(reason)
    }

    /// Detect whether a tool response indicates an error using structured fields first,
    /// falling back to keyword heuristics with false-positive guards.
    fn detect_tool_error(response: &serde_json::Value) -> bool {
        if let Some(obj) = response.as_object() {
            // Priority 1: Structural indicators (most reliable)
            if let Some(ok) = obj.get("ok").and_then(|v| v.as_bool()) {
                return !ok;
            }
            if let Some(code) = obj.get("exit_code").and_then(|v| v.as_i64()) {
                return code != 0;
            }
            if let Some(success) = obj.get("success").and_then(|v| v.as_bool()) {
                return !success;
            }
            if let Some(status) = obj.get("status").and_then(|v| v.as_str()) {
                let s = status.to_lowercase();
                if s == "error" || s == "failed" {
                    return true;
                }
                if s == "ok" || s == "success" {
                    return false;
                }
            }
        }

        // Priority 2: Keyword heuristic with false-positive exclusions
        let result_str = response.to_string().to_lowercase();

        // Short-circuit: if the response is small and contains no signal, it's likely OK
        if result_str.len() < 20 {
            return false;
        }

        let has_error_keyword = result_str.contains("error:")
            || result_str.contains("failed:")
            || result_str.contains("panicked at")
            || result_str.contains("exception:")
            || result_str.contains("traceback ");

        // False-positive guards
        let is_false_positive = result_str.contains("no error")
            || result_str.contains("0 errors")
            || result_str.contains("error_handler")
            || result_str.contains("error.rs")
            || result_str.contains("errors found: 0")
            || result_str.contains("without error");

        has_error_keyword && !is_false_positive
    }

    /// Summarize tool arguments into a brief string for compaction.
    fn summarize_tool_args(tool_name: &str, args: &serde_json::Value) -> String {
        if let Some(obj) = args.as_object() {
            match tool_name {
                "read_file" | "write_file" | "patch_file" => obj
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string(),
                "execute_bash" => {
                    let cmd = obj.get("command").and_then(|v| v.as_str()).unwrap_or("?");
                    Self::truncate_chars(cmd, 80)
                }
                "web_fetch" => obj
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string(),
                "browser" => {
                    let action = obj.get("action").and_then(|v| v.as_str()).unwrap_or("?");
                    let url = obj.get("url").and_then(|v| v.as_str()).unwrap_or("");
                    format!("{} {}", action, Self::truncate_chars(url, 60))
                }
                "task_plan" => obj
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string(),
                _ => {
                    let s = args.to_string();
                    Self::truncate_chars(&s, 60)
                }
            }
        } else {
            Self::truncate_chars(&args.to_string(), 60)
        }
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
                    thought_signature: None,
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
                let char_count = response_str.chars().count();

                if char_count <= max_chars {
                    continue;
                }

                let keep_half = max_chars / 2;
                let head: String = response_str.chars().take(keep_half).collect();
                let tail: String = response_str.chars().skip(char_count - keep_half).collect();

                fr.response = serde_json::json!({
                    "result": format!(
                        "{}\n... [Truncated by context recovery: {} chars hidden] ...\n{}",
                        head,
                        char_count - max_chars,
                        tail
                    ),
                    "original_chars": char_count
                });
                truncated_parts += 1;
            }
        }

        truncated_parts
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
        let bpe = tiktoken_rs::cl100k_base().unwrap();
        let (mut messages, history_tokens_used, history_turns_included, _) =
            self.build_history_with_budget();
        let mut current_turn_tokens = 0;
        if let Some(turn) = &self.current_turn {
            if let Some(sanitized_turn) = Self::sanitize_turn(turn) {
                // Self-Adaptive Context (SAC): Inject [CURRENT TASK] separator
                // so the LLM can clearly distinguish the active goal from historical background.
                let separator = Message {
                    role: "user".to_string(),
                    parts: vec![Part {
                        text: Some("--- [CURRENT TASK] ---".to_string()),
                        function_call: None,
                        function_response: None,
                        thought_signature: None,
                    }],
                };
                current_turn_tokens += Self::estimate_tokens(&bpe, &separator);
                messages.push(separator);

                current_turn_tokens += sanitized_turn
                    .messages
                    .iter()
                    .map(|m| Self::estimate_tokens(&bpe, m))
                    .sum::<usize>();
                messages.extend(sanitized_turn.messages);
            }
        }

        let mut system_static = Vec::new();
        system_static.push(self.system_prompts.join("\n\n"));

        let mut runtime = format!(
            "OS: {}\nArchitecture: {}\n",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        if let Ok(dir) = std::env::current_dir() {
            runtime.push_str(&format!("Current Directory: {}\n", dir.display()));
        }
        system_static.push(format!("## Runtime Environment\n{}", runtime));

        if let Ok(custom) = fs::read_to_string(".claw_prompt.md") {
            system_static.push(format!("## Custom Instructions\n{}", custom));
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
        system_static.push(format!("## Project Context\n{}", project_context));

        let durable_memory = fs::read_to_string("MEMORY.md").ok();

        let mut active_evidence = self.active_evidence.clone();
        if let Some(mem) = &self.retrieved_memory {
            active_evidence.push(crate::evidence::Evidence::new(
                "legacy_rag".into(),
                "memory".into(),
                "retrieved".into(),
                1.0,
                "Retrieved memory snippets".into(),
                mem.clone(),
            ));
        }

        // We leverage native LLM tools array, so tool_schemas text is omitted or simplified
        let (assembled_system_text, report_data) = assembler.assemble_prompt(
            &system_static.join("\n\n"),
            "",
            durable_memory.as_deref(),
            task_state,
            active_evidence,
            Vec::new(), // Not passing transcript tail flatly as we pass via Vec<Message> to preserve function APIs
        );

        let system_msg = Message {
            role: "system".to_string(),
            parts: vec![Part {
                thought_signature: None,
                text: Some(assembled_system_text),
                function_call: None,
                function_response: None,
            }],
        };

        let system_prompt_tokens = report_data.used_tokens;
        let retrieved_memory_snippets = self.retrieved_memory_sources.len();

        // The stats are slightly approximated since we delegated to Assembler
        let report = PromptReport {
            max_history_tokens: self.max_history_tokens,
            history_tokens_used,
            history_turns_included,
            current_turn_tokens,
            system_prompt_tokens,
            total_prompt_tokens: history_tokens_used + current_turn_tokens + system_prompt_tokens,
            retrieved_memory_snippets,
            retrieved_memory_sources: self.retrieved_memory_sources.clone(),
            detailed_stats: self.get_detailed_stats(None),
        };

        (messages, Some(system_msg), report)
    }

    pub fn format_diff(&self, diff: &ContextDiff) -> String {
        let mut output = String::new();
        output.push_str("\n\x1b[1;36m=== Context Diff ===\x1b[0m\n");

        // Token Delta
        let token_sign = if diff.token_delta >= 0 { "+" } else { "" };
        let token_color = if diff.token_delta > 0 {
            "\x1b[31m"
        } else if diff.token_delta < 0 {
            "\x1b[32m"
        } else {
            "\x1b[0m"
        };
        output.push_str(&format!(
            "  Tokens:       {}{}{}\x1b[0m\n",
            token_color, token_sign, diff.token_delta
        ));

        // Truncated Delta
        let trunc_sign = if diff.truncated_delta >= 0 { "+" } else { "" };
        let trunc_color = if diff.truncated_delta > 0 {
            "\x1b[31m"
        } else if diff.truncated_delta < 0 {
            "\x1b[32m"
        } else {
            "\x1b[0m"
        };
        output.push_str(&format!(
            "  Truncated:    {}{}{}\x1b[0m chars\n",
            trunc_color, trunc_sign, diff.truncated_delta
        ));

        // History Turns
        let turn_sign = if diff.history_turns_delta >= 0 {
            "+"
        } else {
            ""
        };
        output.push_str(&format!(
            "  History:      {}{}\x1b[0m turns\n",
            turn_sign, diff.history_turns_delta
        ));

        // System Prompt
        if diff.system_prompt_changed {
            output.push_str("  System:       \x1b[33mCHANGED\x1b[0m\n");
        } else {
            output.push_str("  System:       Unchanged\n");
        }

        // Memory
        if diff.memory_changed {
            output.push_str("  Memory:       \x1b[33mCHANGED\x1b[0m\n");
            for src in &diff.new_sources {
                output.push_str(&format!("    + {}\n", src));
            }
            for src in &diff.removed_sources {
                output.push_str(&format!("    - {}\n", src));
            }
        } else {
            output.push_str("  Memory:       Unchanged\n");
        }

        output
    }

    pub fn inspect_context(&self, section: &str, arg: Option<&str>) -> String {
        match section {
            "system" => self.build_system_prompt(),
            "history" => {
                let count = arg.and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
                let start = self.dialogue_history.len().saturating_sub(count);
                let mut output = String::new();
                for (i, turn) in self.dialogue_history.iter().enumerate().skip(start) {
                    output.push_str(&format!(
                        "\n\x1b[1;33m[Turn {} - {}]\x1b[0m\n",
                        i + 1,
                        turn.turn_id
                    ));
                    output.push_str(&format!("User: {}\n", turn.user_message));
                    output.push_str(&format!("Messages: {}\n", turn.messages.len()));
                }
                if let Some(current) = &self.current_turn {
                    output.push_str(&format!(
                        "\n\x1b[1;32m[Current Turn - {}]\x1b[0m\n",
                        current.turn_id
                    ));
                    output.push_str(&format!("User: {}\n", current.user_message));
                    output.push_str(&format!("Messages: {}\n", current.messages.len()));
                }
                output
            }
            "memory" => {
                if let Some(mem) = &self.retrieved_memory {
                    format!("Sources: {:?}\n\n{}", self.retrieved_memory_sources, mem)
                } else {
                    "No memory retrieved.".to_string()
                }
            }

            _ => format!("Unknown section: {}", section),
        }
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
                thought_signature: None,
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
}
