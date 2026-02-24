use crate::context::{AgentContext, FunctionResponse, Message, Part, PromptReport, Turn};
use crate::llm_client::{GeminiClient, StreamEvent};
use crate::tools::Tool;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[async_trait]
pub trait AgentOutput: Send + Sync {
    async fn on_text(&self, text: &str);
    async fn on_tool_start(&self, name: &str, args: &str);
    async fn on_tool_end(&self, result: &str);
    async fn on_error(&self, error: &str);
}

pub struct AgentLoop {
    llm: Arc<GeminiClient>,
    tools: Vec<Arc<dyn Tool>>,
    context: AgentContext,
    output: Arc<dyn AgentOutput>,
}

#[derive(Debug, Clone, Default)]
struct TaskState {
    goal: String,
    iterations: usize,
    completed: bool,
    last_model_text: String,
    last_error: Option<String>,
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
    const COMPACTION_TRIGGER_RATIO_NUM: usize = 80;
    const COMPACTION_TRIGGER_RATIO_DEN: usize = 100;
    const COMPACTION_TARGET_RATIO_NUM: usize = 25;
    const COMPACTION_TARGET_RATIO_DEN: usize = 100;
    const COMPACTION_MIN_TURNS: usize = 3;
    const MAX_COMPACTION_ATTEMPTS_PER_STEP: usize = 2;
    const MAX_TASK_ITERATIONS: usize = 6;
    const MAX_AUTO_RECOVERY_ATTEMPTS: usize = 2;

    pub fn new(
        llm: Arc<GeminiClient>,
        tools: Vec<Arc<dyn Tool>>,
        context: AgentContext,
        output: Arc<dyn AgentOutput>,
    ) -> Self {
        Self {
            llm,
            tools,
            context,
            output,
        }
    }

    fn should_emit_prompt_report() -> bool {
        std::env::var("CLAW_PROMPT_REPORT").unwrap_or_default() == "1"
    }

    fn should_run_memory_retrieval(query: &str) -> bool {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return false;
        }

        // Skip obvious chit-chat/ack turns to reduce latency.
        let lower = trimmed.to_lowercase();
        let small_talk = [
            "ok", "okay", "thanks", "thank you", "great", "nice", "cool", "yes", "no", "好的",
            "谢谢", "明白", "很好", "收到",
        ];
        if small_talk.iter().any(|w| lower == *w) {
            return false;
        }

        // Run retrieval only for task-like prompts or long requests.
        let task_markers = [
            "bug", "error", "fix", "build", "test", "cargo", "compile", "stack", "trace",
            "function", "file", "path", "schema", "tool", "代码", "修复", "实现", "分析", "报错",
            "测试", "编译", "文件", "路径", "工具", "问题",
        ];
        if task_markers.iter().any(|m| lower.contains(m)) {
            return true;
        }

        trimmed.chars().count() >= 40
    }

    fn enabled_recovery_rules() -> HashSet<String> {
        let configured = std::env::var("CLAW_RECOVERY_RULES").unwrap_or_else(|_| "all".to_string());
        configured
            .split(',')
            .map(|v| v.trim().to_lowercase())
            .filter(|v| !v.is_empty())
            .collect()
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
            let cmd = command
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            format!(
                "command -v {0} || which {0} || echo 'missing command: {0}'",
                cmd
            )
        }
        fn missing_path_match(_cmd: &str, output: &str) -> bool {
            output.to_lowercase().contains("no such file or directory")
        }
        fn missing_path_fix(command: &str, _output: &str) -> String {
            let mut parts = command.split_whitespace();
            if let Some(head) = parts.next() {
                if head == "cat" {
                    if let Some(path) = parts.next() {
                        let parent = Path::new(path)
                            .parent()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| ".".to_string());
                        return format!("pwd && ls -la {}", parent);
                    }
                }
            }
            "pwd && ls -la".to_string()
        }
        fn cargo_toml_match(_cmd: &str, output: &str) -> bool {
            output
                .to_lowercase()
                .contains("could not find `cargo.toml`")
        }
        fn cargo_toml_fix(_cmd: &str, _output: &str) -> String {
            "pwd && ls -la && find .. -maxdepth 3 -name Cargo.toml".to_string()
        }

        vec![
            RecoveryRule {
                name: "missing_command",
                matcher: missing_command_match,
                build_command: missing_command_fix,
            },
            RecoveryRule {
                name: "missing_path",
                matcher: missing_path_match,
                build_command: missing_path_fix,
            },
            RecoveryRule {
                name: "missing_cargo_toml",
                matcher: cargo_toml_match,
                build_command: cargo_toml_fix,
            },
        ]
    }

    fn choose_recovery_rule(
        original_command: &str,
        output: &str,
        enabled_rules: &HashSet<String>,
    ) -> Option<(String, String)> {
        for rule in Self::recovery_rules() {
            let enabled = enabled_rules.contains("all")
                || enabled_rules.contains(&rule.name.to_string().to_lowercase());
            if enabled && (rule.matcher)(original_command, output) {
                return Some((
                    rule.name.to_string(),
                    (rule.build_command)(original_command, output),
                ));
            }
        }
        None
    }

    async fn execute_tool_call_with_recovery(
        &self,
        tool_name: &str,
        tool_args: &Value,
        task_state: &mut TaskState,
    ) -> StructuredToolResult {
        let Some(tool) = self.tools.iter().find(|t| t.name() == tool_name) else {
            return StructuredToolResult {
                ok: false,
                tool_name: tool_name.to_string(),
                output: format!("Error: Tool '{}' not found", tool_name),
                exit_code: None,
                duration_ms: None,
                truncated: false,
                recovery_attempted: false,
                recovery_output: None,
                recovery_rule: None,
            };
        };

        let raw = match tool.execute(tool_args.clone()).await {
            Ok(res) => res,
            Err(e) => format!("Error: {}", e),
        };
        let mut parsed = Self::parse_structured_tool_result(tool_name, &raw);
        if parsed.ok || tool_name != "execute_bash" {
            return parsed;
        }

        if task_state.recovery_attempts >= Self::MAX_AUTO_RECOVERY_ATTEMPTS {
            return parsed;
        }

        let original_command = tool_args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let enabled_rules = Self::enabled_recovery_rules();
        let Some((recovery_rule, recovery_command)) =
            Self::choose_recovery_rule(original_command, &parsed.output, &enabled_rules)
        else {
            return parsed;
        };

        task_state.recovery_attempts += 1;
        *task_state
            .recovery_rule_hits
            .entry(recovery_rule.clone())
            .or_insert(0) += 1;
        let recovery_raw = match tool
            .execute(serde_json::json!({
                "command": recovery_command,
                "timeout": 20
            }))
            .await
        {
            Ok(res) => res,
            Err(e) => format!("Error: {}", e),
        };
        let recovery_parsed = Self::parse_structured_tool_result("execute_bash", &recovery_raw);
        parsed.recovery_attempted = true;
        parsed.recovery_output = Some(recovery_parsed.output.clone());
        parsed.recovery_rule = Some(recovery_rule);
        if recovery_parsed.ok {
            parsed.ok = true;
        }

        parsed
    }

    fn is_task_complete(state: &TaskState) -> bool {
        if state.completed {
            return true;
        }
        let text = state.last_model_text.to_lowercase();
        let done_markers = ["done", "completed", "fixed", "resolved", "successfully"];
        done_markers.iter().any(|m| text.contains(m)) && !text.contains("need to")
    }

    fn build_next_action_prompt(state: &TaskState) -> String {
        format!(
            "Continue solving the same task. Goal: {}. This is iteration {}. \
Use tools proactively and perform one concrete next action. If complete, clearly say DONE and summarize verification.",
            state.goal, state.iterations
        )
    }

    fn rewrite_memory_query(query: &str) -> String {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return String::new();
        }

        // Keep original query and append compact keywords to improve hybrid retrieval.
        let stopwords: HashSet<&'static str> = [
            "the",
            "and",
            "for",
            "with",
            "that",
            "this",
            "from",
            "into",
            "have",
            "has",
            "are",
            "you",
            "your",
            "about",
            "what",
            "when",
            "where",
            "which",
            "will",
            "please",
            "帮我",
            "一下",
            "这个",
            "最近",
            "相关",
            "一下子",
        ]
        .into_iter()
        .collect();

        let mut keywords = Vec::new();
        for token in trimmed
            .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '/' && c != '.')
        {
            let token = token.trim();
            if token.len() < 3 {
                continue;
            }
            let lower = token.to_lowercase();
            if stopwords.contains(lower.as_str()) {
                continue;
            }
            if !keywords.iter().any(|k| k == &lower) {
                keywords.push(lower);
            }
            if keywords.len() >= 8 {
                break;
            }
        }

        if keywords.is_empty() {
            return trimmed.to_string();
        }

        format!("{trimmed}\nkeywords: {}", keywords.join(" "))
    }

    fn parse_retrieval_sources(text: &str) -> Vec<String> {
        let mut sources = Vec::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("--- Source: ") {
                let source = rest
                    .split(" (Relevance:")
                    .next()
                    .unwrap_or(rest)
                    .trim()
                    .to_string();
                if !source.is_empty() && !sources.contains(&source) {
                    sources.push(source);
                }
            }
        }
        sources
    }

    async fn hydrate_retrieved_memory(&mut self, query: &str) -> (String, Vec<String>) {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            self.context.set_retrieved_memory(None, Vec::new());
            return (String::new(), Vec::new());
        }
        if !Self::should_run_memory_retrieval(trimmed) {
            self.context.set_retrieved_memory(None, Vec::new());
            return (String::new(), Vec::new());
        }

        let maybe_tool = self
            .tools
            .iter()
            .find(|t| t.name() == "search_knowledge_base")
            .cloned();

        let Some(tool) = maybe_tool else {
            self.context.set_retrieved_memory(None, Vec::new());
            return (String::new(), Vec::new());
        };

        let retrieval_start = Instant::now();
        self.output
            .on_text("[System] Retrieving relevant memory...\n")
            .await;
        let rewritten_query = Self::rewrite_memory_query(trimmed);
        let result = tool
            .execute(serde_json::json!({
                "query": rewritten_query,
                "limit": 3
            }))
            .await;
        let retrieval_elapsed = retrieval_start.elapsed();
        if retrieval_elapsed >= Duration::from_millis(800) {
            self.output
                .on_text(&format!(
                    "[System] Memory retrieval finished in {} ms.\n",
                    retrieval_elapsed.as_millis()
                ))
                .await;
        }

        match result {
            Ok(text) if !text.contains("No relevant information found") => {
                let capped: String = text.chars().take(2400).collect();
                let sources = Self::parse_retrieval_sources(&capped);
                self.context
                    .set_retrieved_memory(Some(capped), sources.clone());
                (rewritten_query, sources)
            }
            _ => {
                self.context.set_retrieved_memory(None, Vec::new());
                (rewritten_query, Vec::new())
            }
        }
    }

    fn build_compaction_prompt(turns: &[Turn]) -> String {
        let mut summary_prompt = "Please summarize the following conversation history into a concise but comprehensive memorandum. Retain all key technical facts, file paths mentioned, decisions made, and pending issues.\n\n".to_string();

        for (i, turn) in turns.iter().enumerate() {
            summary_prompt.push_str(&format!("--- Turn {} ---\n", i + 1));
            summary_prompt.push_str(&format!("User: {}\n", turn.user_message));
            for msg in &turn.messages {
                if msg.role == "model" {
                    for part in &msg.parts {
                        if let Some(text) = &part.text {
                            summary_prompt.push_str(&format!("Agent: {}\n", text));
                        }
                        if let Some(fc) = &part.function_call {
                            summary_prompt.push_str(&format!("Agent called tool '{}'\n", fc.name));
                        }
                    }
                } else if msg.role == "function" {
                    summary_prompt.push_str("Agent received tool results.\n");
                }
            }
            summary_prompt.push('\n');
        }

        summary_prompt
    }

    async fn maybe_compact_history(&mut self) -> Option<String> {
        let trigger_tokens = self.context.max_history_tokens * Self::COMPACTION_TRIGGER_RATIO_NUM
            / Self::COMPACTION_TRIGGER_RATIO_DEN;
        let target_tokens = self.context.max_history_tokens * Self::COMPACTION_TARGET_RATIO_NUM
            / Self::COMPACTION_TARGET_RATIO_DEN;

        let mut compaction_reason = None;
        for _ in 0..Self::MAX_COMPACTION_ATTEMPTS_PER_STEP {
            let history_tokens = self.context.dialogue_history_token_estimate();
            if history_tokens <= trigger_tokens
                || self.context.dialogue_history.len() < Self::COMPACTION_MIN_TURNS
            {
                break;
            }

            let drain_count = self
                .context
                .oldest_turns_for_compaction(target_tokens, Self::COMPACTION_MIN_TURNS);
            if drain_count == 0 || drain_count >= self.context.dialogue_history.len() {
                break;
            }

            compaction_reason = Some(format!(
                "history_tokens={} exceeded trigger={} (max={})",
                history_tokens, trigger_tokens, self.context.max_history_tokens
            ));
            self.output
                .on_text("\n[System: Auto-compacting history due to token pressure...]\n")
                .await;

            let oldest_turns: Vec<_> = self
                .context
                .dialogue_history
                .drain(0..drain_count)
                .collect();
            let summary_prompt = Self::build_compaction_prompt(&oldest_turns);

            let sys_msg = Message {
                role: "system".to_string(),
                parts: vec![Part {
                    text: Some("You are an expert summarization agent. Your job is to compress conversation history without losing technical details.".to_string()),
                    function_call: None,
                    function_response: None,
                }],
            };

            let user_msg = Message {
                role: "user".to_string(),
                parts: vec![Part {
                    text: Some(summary_prompt),
                    function_call: None,
                    function_response: None,
                }],
            };

            match self.llm.generate_text(vec![user_msg], Some(sys_msg)).await {
                Ok(summary) => {
                    let compacted_turn = Turn {
                        turn_id: uuid::Uuid::new_v4().to_string(),
                        user_message: "SYSTEM: Old conversation history".to_string(),
                        messages: vec![
                            Message {
                                role: "user".to_string(),
                                parts: vec![Part {
                                    text: Some("What happened earlier?".to_string()),
                                    function_call: None,
                                    function_response: None,
                                }],
                            },
                            Message {
                                role: "model".to_string(),
                                parts: vec![Part {
                                    text: Some(format!(
                                        "Earlier conversation summary:\n{}",
                                        summary
                                    )),
                                    function_call: None,
                                    function_response: None,
                                }],
                            },
                        ],
                    };

                    self.context.dialogue_history.insert(0, compacted_turn);
                    self.output
                        .on_text("[System: Compaction complete.]\n\n")
                        .await;
                }
                Err(e) => {
                    self.output
                        .on_error(&format!("\n[Compaction Error]: {}\n", e))
                        .await;
                    for (i, turn) in oldest_turns.into_iter().enumerate() {
                        self.context.dialogue_history.insert(i, turn);
                    }
                    break;
                }
            }
        }
        compaction_reason
    }

    async fn emit_prompt_report(
        &self,
        report: &PromptReport,
        rewritten_query: &str,
        compaction_reason: Option<&str>,
    ) {
        if !Self::should_emit_prompt_report() {
            return;
        }
        let sources = if report.retrieved_memory_sources.is_empty() {
            "none".to_string()
        } else {
            report.retrieved_memory_sources.join(", ")
        };
        let compaction = compaction_reason.unwrap_or("none");
        let text = format!(
            "\n[Prompt Report] total={} system={} history={} (turns={} budget={}) current={} rag_snippets={} sources={} query=\"{}\" compaction={}\n",
            report.total_prompt_tokens,
            report.system_prompt_tokens,
            report.history_tokens_used,
            report.history_turns_included,
            report.max_history_tokens,
            report.current_turn_tokens,
            report.retrieved_memory_snippets,
            sources,
            rewritten_query.replace('\n', " "),
            compaction
        );
        self.output.on_text(&text).await;
    }

    async fn emit_recovery_stats(&self, state: &TaskState) {
        if !Self::should_emit_prompt_report() || state.recovery_attempts == 0 {
            return;
        }
        let mut stats = Vec::new();
        for (rule, hits) in &state.recovery_rule_hits {
            let ratio = (*hits as f64) / (state.recovery_attempts as f64) * 100.0;
            stats.push(format!(
                "{}={}/{} ({:.1}%)",
                rule, hits, state.recovery_attempts, ratio
            ));
        }
        stats.sort();
        let text = format!("[Recovery Stats] {}\n", stats.join(", "));
        self.output.on_text(&text).await;
    }

    pub async fn step(
        &mut self,
        user_input: String,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (rewritten_query, _sources) = self.hydrate_retrieved_memory(&user_input).await;
        let compaction_reason = self.maybe_compact_history().await;
        self.context.start_turn(user_input);
        let mut task_state = TaskState {
            goal: self
                .context
                .current_turn
                .as_ref()
                .map(|t| t.user_message.clone())
                .unwrap_or_default(),
            ..TaskState::default()
        };
        let mut prompt_report_emitted = false;
        let mut had_tool_activity = false;
        for _ in 0..Self::MAX_TASK_ITERATIONS {
            task_state.iterations += 1;
            let (history, system_instruction, prompt_report) = self.context.build_llm_payload();
            if !prompt_report_emitted {
                self.emit_prompt_report(
                    &prompt_report,
                    &rewritten_query,
                    compaction_reason.as_deref(),
                )
                .await;
                prompt_report_emitted = true;
            }

            let mut rx = self
                .llm
                .stream(history.clone(), system_instruction, self.tools.clone())
                .await?;

            let mut full_text = String::new();
            let mut tool_calls = Vec::new();
            let mut waiting_heartbeat_count = 0usize;

            // print!("Rusty-Claw: ");
            // use std::io::Write;
            // std::io::stdout().flush()?;

            loop {
                let event = match tokio::time::timeout(Duration::from_secs(8), rx.recv()).await {
                    Ok(ev) => ev,
                    Err(_) => {
                        waiting_heartbeat_count += 1;
                        self.output
                            .on_text(&format!(
                                "[System] Still working... ({}s elapsed)\n",
                                waiting_heartbeat_count * 8
                            ))
                            .await;
                        continue;
                    }
                };

                let Some(event) = event else {
                    break;
                };
                match event {
                    StreamEvent::Text(text) => {
                        self.output.on_text(&text).await;
                        // print!("{}", text);
                        // std::io::stdout().flush()?;
                        full_text.push_str(&text);
                    }
                    StreamEvent::ToolCall(call) => {
                        tool_calls.push(call);
                    }
                    StreamEvent::Error(e) => {
                        self.output.on_error(&format!("\n[LLM Error]: {}", e)).await;
                        // println!("\n[LLM Error]: {}", e);
                        return Err(e.into());
                    }
                    StreamEvent::Done => break,
                }
            }
            // println!();

            // Record assistant message
            let mut parts = Vec::new();
            if !full_text.is_empty() {
                parts.push(Part {
                    text: Some(full_text.clone()),
                    function_call: None,
                    function_response: None,
                });
            }
            for call in &tool_calls {
                parts.push(Part {
                    text: None,
                    function_call: Some(call.clone()),
                    function_response: None,
                });
            }
            if !parts.is_empty() {
                self.context.add_message_to_current_turn(Message {
                    role: "model".to_string(),
                    parts,
                });
            }

            if tool_calls.is_empty() {
                task_state.last_model_text = full_text.clone();
                task_state.completed = Self::is_task_complete(&task_state);
                // Keep chatty turns snappy: if no tool was used in this turn, stop immediately.
                if !had_tool_activity {
                    break;
                }
                if task_state.completed {
                    break;
                }
                if task_state.iterations >= Self::MAX_TASK_ITERATIONS {
                    self.output
                        .on_error(
                            "[Task] Reached max task iterations. Stopping with partial progress.",
                        )
                        .await;
                    break;
                }

                let continuation = Self::build_next_action_prompt(&task_state);
                self.context.add_message_to_current_turn(Message {
                    role: "user".to_string(),
                    parts: vec![Part {
                        text: Some(continuation),
                        function_call: None,
                        function_response: None,
                    }],
                });
                continue;
            }

            // Execute tools
            let mut response_parts = Vec::new();
            for call in tool_calls {
                had_tool_activity = true;
                let tool_name = call.name.clone();
                let tool_args = call.args.clone();

                self.output
                    .on_tool_start(&tool_name, &tool_args.to_string())
                    .await;
                let tool_result = self
                    .execute_tool_call_with_recovery(&tool_name, &tool_args, &mut task_state)
                    .await;
                if !tool_result.ok {
                    task_state.last_error = Some(tool_result.output.clone());
                }
                let result_str = if tool_result.recovery_attempted {
                    format!(
                        "{}\n[Auto-Recovery]\n{}",
                        tool_result.output,
                        tool_result.recovery_output.clone().unwrap_or_default()
                    )
                } else {
                    tool_result.output.clone()
                };

                self.output.on_tool_end(&result_str).await;

                response_parts.push(Part {
                    text: None,
                    function_call: None,
                    function_response: Some(FunctionResponse {
                        name: tool_name,
                        response: serde_json::json!({
                            "result": result_str,
                            "structured": tool_result
                        }),
                    }),
                });
            }

            self.context.add_message_to_current_turn(Message {
                role: "function".to_string(),
                parts: response_parts,
            });

            // Loop back to give LLM the tool results
        }

        self.context.end_turn();
        self.emit_recovery_stats(&task_state).await;
        Ok(())
    }
}
