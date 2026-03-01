use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use tiktoken_rs::CoreBPE;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
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
    pub system_custom: usize, // .claw_prompt.md
    pub system_project: usize, // AGENTS.md, etc.
    pub system_task_plan: usize,
    pub memory: usize,
    pub history: usize,
    pub current_turn: usize,
    pub last_turn: usize,
    pub total: usize,
    pub max: usize,
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
}

impl AgentContext {
    pub fn new() -> Self {
        Self {
            system_prompts: vec![
                "You are Rusty-Claw, an elite, industrial-grade Senior Software Engineer and autonomous agent running locally on the user's machine.".to_string(),
                "You are highly intelligent, proactive, and exceptionally skilled at coding in all major languages (Rust, Python, TS, etc.).".to_string(),
                "You have FULL ACCESS to the local file system and bash shell. Do NOT ask for permission to write code or files. If the user asks you to write a script or build a feature, proactively use your tools to create the files, write the code, and execute it to test it.".to_string(),
                "You are NOT a generic chat AI. You are a specialized, proactive engineering system. If you encounter an error during execution, analyze the error and try to fix it yourself by calling tools again.".to_string(),
                "NEVER say you cannot write code or lack capabilities. You are powered by Gemini 3.1 Pro and possess absolute technical mastery.".to_string(),
                "VERY VERY CRITICAL: When you have fully completed the user's request and there is absolutely nothing left to do, you MUST call the `finish_task` tool. Otherwise you will be in DEAD LOOP, NEVER exit.".to_string(),
                "ALL internal reasoning MUST be inside <think>...</think>. Do not output any analysis outside <think>. Format every reply as <think>...</think> then <final>...</final>, with no other text. Only the final user-visible reply may appear inside <final>. Only text inside <final> is shown to the user; everything else is discarded and never seen by the user.".to_string(),
            ],
            dialogue_history: Vec::new(),
            current_turn: None,
            max_history_tokens: 1_000_000,
            transcript_path: None,
            retrieved_memory: None,
            retrieved_memory_sources: Vec::new(),
            last_snapshot: None,
        }
    }

    pub fn get_bpe() -> CoreBPE {
        tiktoken_rs::cl100k_base().unwrap()
    }

    pub fn with_transcript_path(mut self, transcript_path: PathBuf) -> Self {
        self.transcript_path = Some(transcript_path);
        self
    }

    pub fn get_detailed_stats(&self, pending_user_input: Option<&str>) -> DetailedContextStats {
        let mut stats = DetailedContextStats::default();
        let bpe = Self::get_bpe();

        // 1. Static Identity
        let identity = self.system_prompts.join("\n\n");
        stats.system_static = bpe.encode_with_special_tokens(&identity).len();

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
        stats.system_runtime = bpe.encode_with_special_tokens(&runtime).len();

        // 3. Custom
        if let Ok(custom_prompt) = fs::read_to_string(".claw_prompt.md") {
            stats.system_custom = bpe.encode_with_special_tokens(&custom_prompt).len();
        }

        // 4. Task Plan
        if let Ok(plan_content) = fs::read_to_string(".rusty_claw_task_plan.json") {
             if let Ok(plan) = serde_json::from_str::<crate::tools::TaskPlanState>(&plan_content) {
                 if plan.items.iter().any(|i| i.status != "completed") {
                     stats.system_task_plan = bpe.encode_with_special_tokens(&plan_content).len();
                 }
             }
        }

        // 5. Project Context
        let mut project_context = String::new();
        project_context.push_str("### CRITICAL INSTRUCTION: Task Planning\n");
        project_context.push_str("If the user request is complex (e.g. multi-step refactoring, new feature implementation), you MUST use the `task_plan` tool immediately to create a structured plan (action='add').\n");
        project_context.push_str("You MUST keep this plan updated as you progress (using action='update_status').\n");
        project_context.push_str("HOWEVER, if the user explicitly issues a new, unrelated command or asks to change direction, you should prioritize the user's new request over the existing plan (ask for confirmation if unsure).\n\n");
        
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
        stats.system_project = bpe.encode_with_special_tokens(&project_context).len();

        // 6. Memory (RAG)
        if let Some(memory) = &self.retrieved_memory {
            stats.memory = bpe.encode_with_special_tokens(memory).len();
        }

        // 7. History (Net Load)
        let (_, history_tokens, _) = self.build_history_with_budget();
        stats.history = history_tokens;

        // 8. Current Turn
        if let Some(turn) = &self.current_turn {
             for msg in &turn.messages {
                 for part in &msg.parts {
                     if let Some(text) = &part.text {
                         stats.current_turn += bpe.encode_with_special_tokens(text).len();
                     }
                     if let Some(fc) = &part.function_call {
                         stats.current_turn += bpe.encode_with_special_tokens(&fc.name).len();
                         stats.current_turn += bpe.encode_with_special_tokens(&fc.args.to_string()).len();
                     }
                     if let Some(fr) = &part.function_response {
                         stats.current_turn += bpe.encode_with_special_tokens(&fr.name).len();
                         stats.current_turn += bpe.encode_with_special_tokens(&fr.response.to_string()).len();
                     }
                 }
             }
        } else if let Some(input) = pending_user_input {
             stats.current_turn = bpe.encode_with_special_tokens(input).len();
        }

        // 9. Last Turn
        if let Some(last) = self.dialogue_history.last() {
            stats.last_turn = Self::turn_token_estimate(last, &bpe);
        }

        // 10. Total
        stats.total = stats.system_static + stats.system_runtime + stats.system_custom + stats.system_project + stats.system_task_plan + stats.memory + stats.history + stats.current_turn;
        stats.max = self.max_history_tokens;

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
            turn_id: self.current_turn.as_ref().map(|t| t.turn_id.clone()).unwrap_or_default(),
            stats,
            messages_count: self.dialogue_history.iter().map(|t| t.messages.len()).sum::<usize>() + self.current_turn.as_ref().map(|t| t.messages.len()).unwrap_or(0),
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

        let old_sources: std::collections::HashSet<_> = old.retrieved_memory_sources.iter().cloned().collect();
        let new_sources_set: std::collections::HashSet<_> = self.retrieved_memory_sources.iter().cloned().collect();

        let new_sources = self.retrieved_memory_sources.iter().filter(|s| !old_sources.contains(*s)).cloned().collect();
        let removed_sources = old.retrieved_memory_sources.iter().filter(|s| !new_sources_set.contains(*s)).cloned().collect();

        ContextDiff {
            token_delta: current_stats.total as i64 - old.stats.total as i64,
            history_turns_delta: self.dialogue_history.len() as i32 - old.history_turns_count as i32,
            system_prompt_changed: current_hash != old.system_prompt_hash,
            new_sources,
            removed_sources,
            memory_changed: self.retrieved_memory_sources != old.retrieved_memory_sources,
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
        if let Ok(plan_content) = fs::read_to_string(".rusty_claw_task_plan.json") {
             if let Ok(plan) = serde_json::from_str::<crate::tools::TaskPlanState>(&plan_content) {
                 if plan.items.iter().any(|i| i.status != "completed") {
                     let mut plan_str = String::new();
                     plan_str.push_str("You MUST follow this plan strictly. Do not skip steps without approval.\n\n");
                     for (i, item) in plan.items.iter().enumerate() {
                         let status_icon = match item.status.as_str() {
                             "completed" => "[x]",
                             "in_progress" => "[IN PROGRESS]",
                             _ => "[ ]",
                         };
                         plan_str.push_str(&format!("{} {}. {}\n", status_icon, i + 1, item.step));
                         if let Some(note) = &item.note {
                             plan_str.push_str(&format!("   Note: {}\n", note));
                         }
                     }
                     if let Some(section) = Self::build_prompt_section("Current Task Plan (STRICT)", plan_str, 4_000) {
                         sections.push(section);
                     }
                 }
             }
        }


        let mut project_context = String::new();
        // Add Task Plan Instruction
        project_context.push_str("### CRITICAL INSTRUCTION: Task Planning\n");
        project_context.push_str("If the user request is complex (e.g. multi-step refactoring, new feature implementation), you MUST use the `task_plan` tool immediately to create a structured plan (action='add').\n");
        project_context.push_str("You MUST keep this plan updated as you progress (using action='update_status').\n");
        project_context.push_str("HOWEVER, if the user explicitly issues a new, unrelated command or asks to change direction, you should prioritize the user's new request over the existing plan (ask for confirmation if unsure).\n\n");
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
                                        let tail: String = s.chars().skip(char_count - keep).collect();
                                        
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
        cloned
    }

    fn is_user_referencing_history(msg: &str) -> bool {
        let lower = msg.to_lowercase();
        let keywords = [
            "previous command", "last command", "previous output", "last output",
            "what did it say", "fix the error", "look above", "check the error",
            "what was the error", "show me the output", "full output"
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
                break;
            }
        }
        result.trim().to_string()
    }

    fn strip_response_payload(fr: &mut FunctionResponse) {
        if let Some(obj) = fr.response.as_object_mut() {
            match fr.name.as_str() {
                "read_file" => {
                    let content_val = if let Some(v) = obj.get_mut("content") {
                        Some(v)
                    } else {
                        obj.get_mut("result")
                    };

                    if let Some(content_val) = content_val {
                        if let Some(s) = content_val.as_str() {
                            let line_count = s.lines().count();
                            if line_count > 10 {
                                let head: String = s.lines().take(5).collect::<Vec<_>>().join("\n");
                                let tail: String = s.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
                                *content_val = serde_json::Value::String(format!(
                                    "{}\n... [History: Content stripped - {} lines total] ...\n{}",
                                    head, line_count, tail
                                ));
                            }
                        }
                    }
                },
                "execute_bash" => {
                    for field in ["stdout", "stderr", "result"] {
                        if let Some(val) = obj.get_mut(field) {
                            if let Some(s) = val.as_str() {
                                let char_count = s.chars().count();
                                if char_count > 500 {
                                    let head: String = s.chars().take(200).collect();
                                    let tail: String = s.chars().skip(char_count - 200).collect();
                                    *val = serde_json::Value::String(format!(
                                        "{}\n... [History: Output stripped - {} chars total] ...\n{}",
                                        head, char_count, tail
                                    ));
                                }
                            }
                        }
                    }
                },
                "ls" | "find" | "grep" => {
                     if let Some(files) = obj.get_mut("files").and_then(|v| v.as_array_mut()) {
                         if files.len() > 10 {
                             let total = files.len();
                             files.truncate(10);
                             files.push(serde_json::Value::String(format!("... and {} more files", total - 10)));
                         }
                     }
                },
                "web_fetch" | "web_search_tavily" => {
                    if let Some(content) = obj.get_mut("result") {
                        if let Some(s) = content.as_str() {
                             *content = serde_json::Value::String(format!(
                                "[History: Web content stripped - {} chars]", s.len()
                            ));
                        }
                    }
                },
                "skill" | "use_skill" => {
                    let msg_val = if let Some(v) = obj.get_mut("message") {
                        Some(v)
                    } else {
                        obj.get_mut("result")
                    };

                    if let Some(msg) = msg_val {
                         *msg = serde_json::Value::String("Skill loaded and active.".to_string());
                    }
                },
                _ => {
                    for (_k, v) in obj.iter_mut() {
                        if let Some(s) = v.as_str() {
                            if s.len() > 1000 {
                                let head: String = s.chars().take(500).collect();
                                let tail: String = s.chars().skip(s.chars().count() - 200).collect();
                                *v = serde_json::Value::String(format!(
                                    "{}\n... [History: Value stripped - {} chars] ...\n{}",
                                    head, s.len(), tail
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    fn reconstruct_turn_for_history(turn: &Turn) -> Turn {
        let mut new_messages = Vec::new();

        for msg in &turn.messages {
            let mut new_parts = Vec::new();

            for part in &msg.parts {
                let mut new_part = Part {
                    text: None,
                    function_call: None,
                    function_response: None,
                    thought_signature: None, // ALWAYS None for history
                };

                // 1. Function Call (Action) - KEEP
                if let Some(fc) = &part.function_call {
                    new_part.function_call = Some(fc.clone());
                }

                // 2. Function Response (Result) - KEEP (Stripped)
                if let Some(fr) = &part.function_response {
                    let mut stripped_fr = fr.clone();
                    Self::strip_response_payload(&mut stripped_fr);
                    new_part.function_response = Some(stripped_fr);
                }

                // 3. Text (Intent/Reply) - SELECTIVE KEEP
                if let Some(text) = &part.text {
                    if msg.role == "user" {
                        // User text: Keep, but clean system tags
                        let mut cleaned_text = text.clone();
                        let markers = [
                            "[SYSTEM NOTE: FOCUS ON THIS NEW USER MESSAGE. Context above is history.]",
                            "[SYSTEM ALERT: CRITICAL INSTRUCTION. IGNORE PREVIOUS CONTEXT IF CONFLICTING.]",
                            "[SYSTEM NOTE: FOCUS ON THIS NEW USER MESSAGE.]"
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
                            let cleaned = Self::strip_thinking_tags(text);
                            if !cleaned.is_empty() {
                                new_part.text = Some(cleaned);
                            }
                        }
                    }
                }
                
                // Only add part if it has content
                if new_part.text.is_some() || new_part.function_call.is_some() || new_part.function_response.is_some() {
                    new_parts.push(new_part);
                }
            }

            if !new_parts.is_empty() {
                 new_messages.push(Message { role: msg.role.clone(), parts: new_parts });
            }
        }
        
        Turn {
            turn_id: turn.turn_id.clone(),
            user_message: turn.user_message.clone(),
            messages: new_messages,
        }
    }

    fn build_history_with_budget(&self) -> (Vec<Message>, usize, usize) {
        let bpe = tiktoken_rs::cl100k_base().unwrap();
        let history_budget = self.max_history_tokens.saturating_mul(85) / 100;
        let mut history_messages = Vec::new();
        let mut current_tokens = 0;
        let mut turns_included = 0;

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

            let turn = if should_strip {
                Self::reconstruct_turn_for_history(&sanitized)
            } else {
                Self::truncate_old_tool_results(&sanitized)
            };
            
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

    // Refactored to return accurate NET tokens (using compression)
    pub fn get_context_status(&self) -> (usize, usize, usize, usize, usize) {
        let bpe = tiktoken_rs::cl100k_base().unwrap();
        
        // 1. Calculate History (Net - Compressed)
        let (_, history_tokens, _) = self.build_history_with_budget();

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
        
        (total_tokens, self.max_history_tokens, history_tokens, current_turn_tokens, system_tokens)
    }

    pub fn get_context_details(&self) -> String {
        let bpe = Self::get_bpe();
        let stats = self.get_detailed_stats(None);
        
        let mut details = String::new();
        details.push_str("\n\x1b[1;36m=== Context Audit Report ===\x1b[0m\n");
        
        details.push_str(&format!("\x1b[1;33m[Token Budget]\x1b[0m  {}/{} tokens ({:.1}% used)\n", 
            stats.total, stats.max, (stats.total as f64 / stats.max as f64) * 100.0));

        details.push_str("\n\x1b[1;33m[System Components]\x1b[0m\n");
        details.push_str(&format!("  - Identity (Static):   {} tokens\n", stats.system_static));
        details.push_str(&format!("  - Runtime Env:        {} tokens\n", stats.system_runtime));
        
        if stats.system_custom > 0 {
            details.push_str(&format!("  - Custom Prompt:      {} tokens (.claw_prompt.md)\n", stats.system_custom));
        }
        
        if stats.system_task_plan > 0 {
            details.push_str(&format!("  - Task Plan:          {} tokens\n", stats.system_task_plan));
        }

        details.push_str(&format!("  - Project Context:    {} tokens\n", stats.system_project));
        let project_files = ["AGENTS.md", "README.md", "MEMORY.md"];
        for file in project_files {
            if let Ok(meta) = fs::metadata(file) {
                details.push_str(&format!("    * {} ({} bytes)\n", file, meta.len()));
            }
        }

        details.push_str("\n\x1b[1;33m[Conversation History]\x1b[0m\n");
        let (_, _, turns_included) = self.build_history_with_budget();
        details.push_str(&format!("  - History Load:       {} tokens ({} turns included)\n", stats.history, turns_included));
        details.push_str(&format!("  - Total History:      {} tokens ({} turns total)\n", self.dialogue_history_token_estimate(), self.dialogue_history.len()));

        if stats.memory > 0 {
            details.push_str("\n\x1b[1;33m[RAG Memory]\x1b[0m\n");
            details.push_str(&format!("  - Retrieved:          {} tokens\n", stats.memory));
            for src in &self.retrieved_memory_sources {
                details.push_str(&format!("    * {}\n", src));
            }
        }

        if let Some(turn) = &self.current_turn {
            details.push_str("\n\x1b[1;33m[Current Turn]\x1b[0m\n");
            details.push_str(&format!("  - Active Payload:     {} tokens\n", stats.current_turn));
            details.push_str(&format!("  - User Message:       {}\n", Self::truncate_chars(&turn.user_message, 80)));
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

    pub fn build_llm_payload(&self) -> (Vec<Message>, Option<Message>, PromptReport) {
        let bpe = tiktoken_rs::cl100k_base().unwrap();
        let (mut messages, history_tokens_used, history_turns_included) =
            self.build_history_with_budget();
        let mut current_turn_tokens = 0;
        if let Some(turn) = &self.current_turn {
            if let Some(mut sanitized_turn) = Self::sanitize_turn(turn) {
                // FOCUS BOOSTER: Reinforce the new instruction based on history length.
                if history_turns_included >= 1 {
                    let booster_msg = if history_turns_included > 20 {
                        "[SYSTEM ALERT: CRITICAL INSTRUCTION. IGNORE PREVIOUS CONTEXT IF CONFLICTING.]"
                    } else if history_turns_included > 10 {
                        "[SYSTEM NOTE: FOCUS ON THIS NEW USER MESSAGE.]"
                    } else {
                        "[SYSTEM NOTE: FOCUS ON THIS NEW USER MESSAGE. Context above is history.]"
                    };

                    if let Some(user_msg) = sanitized_turn.messages.iter_mut().find(|m| m.role == "user") {
                        if let Some(part) = user_msg.parts.first_mut() {
                            if let Some(text) = &mut part.text {
                                *text = format!("{}\n\n{}", booster_msg, text);
                            }
                        }
                    }
                }

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
                    thought_signature: None,
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
            detailed_stats: self.get_detailed_stats(None),
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

        let (payload, _sys, _report) = ctx.build_llm_payload();
        assert_eq!(payload.len(), 1);
        assert_eq!(
            payload.last().unwrap().parts[0].text.as_ref().unwrap(),
            "Short message"
        );
    }
}
