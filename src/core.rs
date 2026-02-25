use crate::context::{
    AgentContext, FunctionCall, FunctionResponse, Message, Part, PromptReport, Turn,
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

#[async_trait]
pub trait AgentOutput: Send + Sync {
    async fn on_text(&self, text: &str);
    async fn on_tool_start(&self, name: &str, args: &str);
    async fn on_tool_end(&self, result: &str);
    async fn on_error(&self, error: &str);
}

use std::sync::atomic::{AtomicBool, Ordering};

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
    completed: bool,
    last_model_text: String,
    last_error: Option<String>,
    recovery_attempts: usize,
    recovery_rule_hits: HashMap<String, usize>,
    
    // Deadlock detection & Dynamic Budget
    energy_points: usize,
    recent_tool_signatures: Vec<String>,
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
    const DEFAULT_MAX_TASK_ITERATIONS: usize = 12;
    const MAX_AUTO_RECOVERY_ATTEMPTS: usize = 2;
    const MAX_LLM_RECOVERY_ATTEMPTS: usize = 3;

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

    fn should_emit_prompt_report() -> bool {
        std::env::var("CLAW_PROMPT_REPORT").unwrap_or_default() == "1"
    }

    fn should_emit_timing_report() -> bool {
        std::env::var("CLAW_TIMING_REPORT")
            .map(|v| v != "0")
            .unwrap_or(true)
    }

    fn should_emit_verbose_progress() -> bool {
        std::env::var("CLAW_VERBOSE_PROGRESS")
            .map(|v| v == "1")
            .unwrap_or(false)
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
            "context",
            "token",
            "too large",
            "exceeds",
            "maximum",
            "request payload size",
            "prompt is too long",
            "input too long",
        ];
        (lower.contains("400") || lower.contains("invalid argument"))
            && markers.iter().any(|m| lower.contains(m))
    }

    fn is_transient_llm_error(err: &str) -> bool {
        let lower = err.to_lowercase();
        let markers = [
            "429",
            "rate limit",
            "resource exhausted",
            "unavailable",
            "deadline",
            "timeout",
            "connection reset",
            "temporarily",
            "503",
            "502",
            "504",
        ];
        markers.iter().any(|m| lower.contains(m))
    }

    fn enforce_final_tag_enabled() -> bool {
        std::env::var("CLAW_ENFORCE_FINAL_TAG").unwrap_or_default() == "1"
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

    fn extract_final_tag_content(text: &str) -> Option<String> {
        let lower = text.to_lowercase();
        let start_marker = "<final>";
        let end_marker = "</final>";
        let start = lower.find(start_marker)?;
        let end = lower[start + start_marker.len()..].find(end_marker)?;
        let content_start = start + start_marker.len();
        let content_end = content_start + end;
        Some(text[content_start..content_end].trim().to_string())
    }

    fn is_message_delivery_tool(name: &str) -> bool {
        let lower = name.to_lowercase();
        lower.contains("send_message")
            || lower.contains("message")
            || lower.contains("reply")
            || lower.contains("notify")
    }

    async fn recover_from_llm_error(&mut self, err: &str, attempt: usize) -> bool {
        if Self::is_context_overflow_error(err) {
            self.output
                .on_text(
                    "[System] LLM context overflow detected. Running compaction and truncating large tool outputs...\n",
                )
                .await;

            let _ = self.maybe_compact_history().await;
            let truncated = self.context.truncate_current_turn_tool_results(2_000);
            if truncated > 0 {
                self.output
                    .on_text(&format!(
                        "[System] Truncated {} oversized tool result(s) in current turn.\n",
                        truncated
                    ))
                    .await;
            }
            // If no truncation happened, dropping retrieved snippets can still reduce prompt size.
            self.context.set_retrieved_memory(None, Vec::new());
            return true;
        }

        if Self::is_transient_llm_error(err) && attempt < Self::MAX_LLM_RECOVERY_ATTEMPTS {
            let backoff_ms = (attempt as u64).saturating_mul(800);
            self.output
                .on_text(&format!(
                    "[System] Transient LLM error. Retrying in {} ms...\n",
                    backoff_ms
                ))
                .await;
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            return true;
        }

        false
    }

    fn should_run_memory_retrieval(query: &str) -> bool {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return false;
        }

        // Skip obvious chit-chat/ack turns to reduce latency.
        let lower = trimmed.to_lowercase();
        let small_talk = [
            "ok",
            "okay",
            "thanks",
            "thank you",
            "great",
            "nice",
            "cool",
            "yes",
            "no",
            "好的",
            "谢谢",
            "明白",
            "很好",
            "收到",
        ];
        if small_talk.iter().any(|w| lower == *w) {
            return false;
        }

        if trimmed.chars().count() < 20 {
            return false;
        }

        let memory_intent_markers = [
            "remember", "recall", "previous", "earlier", "history", "memory", "之前", "上次",
            "历史", "记住", "回忆",
        ];
        if memory_intent_markers.iter().any(|m| lower.contains(m)) {
            return true;
        }

        // Run retrieval only for strongly task-like prompts.
        let task_markers = [
            "bug", "error", "fix", "build", "test", "cargo", "compile", "stack", "trace",
            "function", "file", "path", "schema", "tool", "代码", "修复", "实现", "分析", "报错",
            "测试", "编译", "文件", "路径", "工具", "问题",
        ];
        if task_markers.iter().any(|m| lower.contains(m)) && trimmed.chars().count() >= 24 {
            return true;
        }

        // Fallback for unusually long, detail-heavy requests.
        trimmed.chars().count() >= 120
    }

    fn max_attempts_for_input(_query: &str) -> usize {
        Self::max_task_iterations()
    }

    async fn emit_timing_report(
        &self,
        total_ms: u128,
        retrieval_ms: u128,
        retrieval_timed_out: bool,
        compaction_ms: u128,
        prompt_build_ms: u128,
        llm_setup_ms: u128,
        llm_stream_wait_ms: u128,
        tool_exec_ms: u128,
        iterations: usize,
        llm_attempts: usize,
        first_event_ms: Option<u128>,
    ) {
        if !Self::should_emit_timing_report() {
            return;
        }
        tracing::info!(
            total_ms = total_ms,
            iterations = iterations,
            llm_attempts = llm_attempts,
            retrieval_ms = retrieval_ms,
            retrieval_timed_out = retrieval_timed_out,
            compaction_ms = compaction_ms,
            prompt_build_ms = prompt_build_ms,
            llm_setup_ms = llm_setup_ms,
            llm_stream_wait_ms = llm_stream_wait_ms,
            tool_exec_ms = tool_exec_ms,
            first_event_ms = first_event_ms.unwrap_or(0),
            "step_timing"
        );
        let first = first_event_ms
            .map(|v| v.to_string())
            .unwrap_or_else(|| "n/a".to_string());
        let retrieval_note = if retrieval_timed_out { "timeout" } else { "ok" };
        self.output
            .on_text(&format!(
                "[Perf] total={}ms iter={} llm_attempts={} retrieval={}ms({}) compaction={}ms prompt={}ms llm_setup={}ms llm_stream={}ms tool_exec={}ms first_event={}ms\n",
                total_ms,
                iterations,
                llm_attempts,
                retrieval_ms,
                retrieval_note,
                compaction_ms,
                prompt_build_ms,
                llm_setup_ms,
                llm_stream_wait_ms,
                tool_exec_ms,
                first
            ))
            .await;
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

    fn summarize_model_intent(text: &str) -> Option<String> {
        let cleaned = text.replace('\n', " ");
        let trimmed = cleaned.trim();
        if trimmed.is_empty() {
            return None;
        }
        let summary: String = trimmed.chars().take(90).collect();
        Some(summary)
    }

    fn tool_purpose(tool_name: &str, tool_args: &Value, model_text: &str) -> String {
        let base = match tool_name {
            "execute_bash" => {
                let cmd = tool_args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("<empty>");
                format!("执行命令并验证结果：`{}`", cmd)
            }
            "read_file" => {
                let path = tool_args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("<unknown>");
                format!("读取文件以确认实现细节：`{}`", path)
            }
            "write_file" => {
                let path = tool_args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("<unknown>");
                format!("写入代码或配置变更：`{}`", path)
            }
            "web_fetch" => {
                let url = tool_args
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("<unknown>");
                format!("抓取网页原文用于回答：`{}`", url)
            }
            "web_search_tavily" => {
                let query = tool_args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .unwrap_or("<unknown>");
                format!("检索外部资料：`{}`", query)
            }
            "task_plan" => "更新任务计划状态并同步进展".to_string(),
            "read_workspace_memory" => "读取工作区记忆以恢复上下文".to_string(),
            "write_workspace_memory" => "记录关键信息到工作区记忆".to_string(),
            "search_knowledge_base" => "检索知识库历史经验".to_string(),
            "memorize_knowledge" => "沉淀新知识到知识库".to_string(),
            _ => "执行下一步动作并收集可验证证据".to_string(),
        };

        if let Some(intent) = Self::summarize_model_intent(model_text) {
            format!("{}；依据：{}", base, intent)
        } else {
            base
        }
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

    async fn execute_retrieval_task(
        tools: Vec<Arc<dyn Tool>>,
        query: String,
        output: Arc<dyn AgentOutput>,
    ) -> (String, Vec<String>) {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return (String::new(), Vec::new());
        }
        if !Self::should_run_memory_retrieval(trimmed) {
            return (String::new(), Vec::new());
        }

        let maybe_tool = tools
            .iter()
            .find(|t| t.name() == "search_knowledge_base")
            .cloned();

        let Some(tool) = maybe_tool else {
            return (String::new(), Vec::new());
        };

        let retrieval_start = Instant::now();
        // Don't print "Retrieving..." here to avoid spamming UI if it finishes fast or times out silently

        let rewritten_query = Self::rewrite_memory_query(trimmed);
        let result = tool
            .execute(serde_json::json!({
                "query": rewritten_query,
                "limit": 3
            }))
            .await;

        let retrieval_elapsed = retrieval_start.elapsed();
        if retrieval_elapsed >= Duration::from_millis(1000) {
            output
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
                (capped, sources)
            }
            _ => (String::new(), Vec::new()),
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
    ) -> Result<RunExit, Box<dyn std::error::Error + Send + Sync>> {
        let _initial_limit = Self::max_task_iterations();
        self.output
            .on_text(&format!(
                "[Progress] 接收任务：{}。最大允许 {} 轮。
",
                user_input.trim(),
                15 // Starting energy
            ))
            .await;
        if Self::should_emit_verbose_progress() {
            self.output
                .on_text("[Progress] 当前阶段：分析问题并准备执行。
")
                .await;
        }
        let step_started = Instant::now();
        let retrieval_started = Instant::now();
        let retrieval_future = Self::execute_retrieval_task(
            self.tools.clone(),
            user_input.clone(),
            self.output.clone(),
        );

        // Race: Give it 800ms max.
        let (retrieved_text, _sources, retrieval_timed_out) =
            match tokio::time::timeout(Duration::from_millis(800), retrieval_future).await {
                Ok(res) => (res.0, res.1, false),
                Err(_) => (String::new(), Vec::new(), true),
            };
        let retrieval_ms = retrieval_started.elapsed().as_millis();
        if Self::should_emit_verbose_progress() {
            if retrieval_timed_out {
                self.output
                    .on_text("[Progress] 检索阶段超时，已跳过检索以保证响应速度。\n")
                    .await;
            } else if self.context.retrieved_memory().is_some() {
                self.output
                    .on_text("[Progress] 检索阶段完成，已注入相关历史上下文。\n")
                    .await;
            }
        }

        self.context.set_retrieved_memory(
            if retrieved_text.is_empty() {
                None
            } else {
                Some(retrieved_text)
            },
            _sources,
        );
        let rewritten_query = if self.context.retrieved_memory().is_some() {
            Self::rewrite_memory_query(&user_input)
        } else {
            String::new()
        };

        let compaction_started = Instant::now();
        let compaction_reason = self.maybe_compact_history().await;
        let compaction_ms = compaction_started.elapsed().as_millis();
        if Self::should_emit_verbose_progress() && compaction_reason.is_some() {
            self.output
                .on_text("[Progress] 上下文已压缩，继续执行主任务。\n")
                .await;
        }

        self.context.start_turn(user_input);
        let mut task_state = TaskState {
            goal: self
                .context
                .current_turn
                .as_ref()
                .map(|t| t.user_message.clone())
                .unwrap_or_default(),
            iterations: 0,
            completed: false,
            last_model_text: String::new(),
            last_error: None,
            recovery_attempts: 0,
            recovery_rule_hits: HashMap::new(),
            energy_points: 20, // Starting energy
            recent_tool_signatures: Vec::new(),
        };
        let mut prompt_report_emitted = false;
        
        let mut exit_state = RunExit::CompletedSilent {
            cause: "run_finished_without_output".to_string(),
        };
        let mut prompt_build_ms_acc: u128 = 0;
        let mut llm_setup_ms_acc: u128 = 0;
        let mut llm_stream_wait_ms_acc: u128 = 0;
        let mut tool_exec_ms_acc: u128 = 0;
        let mut llm_attempts_total: usize = 0;
        let mut first_event_ms_first_iter: Option<u128> = None;
        
        
        self.cancel_token.store(false, Ordering::SeqCst);

        while task_state.energy_points > 0 {
            if self.cancel_token.load(Ordering::SeqCst) {
                self.output.on_text("\n\x1b[33m⚠️ [System] 任务已被用户强制挂起！您可以输入新的指示来继续。\x1b[0m\n").await;
                exit_state = RunExit::YieldedToUser;
                break;
            }
            
            task_state.iterations += 1;
            task_state.energy_points = task_state.energy_points.saturating_sub(1);
            if Self::should_emit_verbose_progress() {
                let objective = "寻找解决方案并执行，或调用 finish_task 结束";
                self.output
                    .on_text(&format!(
                        "[Progress] 当前阶段：第 {} 轮 (剩余精力: {})，目标：{}。\n",
                        task_state.iterations, task_state.energy_points, objective
                    ))
                    .await;
            }
            let prompt_build_started = Instant::now();
            let (history, system_instruction, prompt_report) = self.context.build_llm_payload();
            prompt_build_ms_acc += prompt_build_started.elapsed().as_millis();
            if !prompt_report_emitted {
                self.emit_prompt_report(
                    &prompt_report,
                    &rewritten_query,
                    compaction_reason.as_deref(),
                )
                .await;
                prompt_report_emitted = true;
            }

            let mut llm_attempt = 0usize;
            let mut stream_error: Option<String> = None;
            let llm_setup_started = Instant::now();
            let mut rx = loop {
                llm_attempt += 1;
                match self
                    .llm
                    .stream(
                        history.clone(),
                        system_instruction.clone(),
                        self.tools.clone(),
                    )
                    .await
                {
                    Ok(rx) => break rx,
                    Err(e) => {
                        let err_text = e.to_string();
                        let can_recover = llm_attempt < Self::MAX_LLM_RECOVERY_ATTEMPTS
                            && self.recover_from_llm_error(&err_text, llm_attempt).await;
                        if can_recover {
                            continue;
                        }
                        stream_error = Some(err_text);
                        break tokio::sync::mpsc::channel(1).1;
                    }
                }
            };
            llm_attempts_total += llm_attempt;
            llm_setup_ms_acc += llm_setup_started.elapsed().as_millis();
            if let Some(err_text) = stream_error {
                self.output
                    .on_error(&format!("\n[LLM Error]: {}", err_text))
                    .await;
                exit_state = RunExit::RecoverableFailed {
                    reason: "llm_stream_setup_failed".to_string(),
                    attempts: llm_attempt,
                };
                break;
            }

            let mut full_text = String::new();
            let mut raw_full_text = String::new();
            let mut tool_calls = Vec::new();
            let mut waiting_heartbeat_count = 0usize;
            let mut saw_no_reply_token = false;
            let mut saw_heartbeat_token = false;
            let mut silent_cause_hint: Option<String> = None;
            let llm_stream_started = Instant::now();
            let mut first_event_recorded = false;

            loop {
                if self.cancel_token.load(Ordering::SeqCst) {
                    self.output.on_text("\n\x1b[33m⚠️ [System] 正在中断 LLM 请求...\x1b[0m\n").await;
                    exit_state = RunExit::YieldedToUser;
                    break;
                }
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
                if !first_event_recorded {
                    let t = llm_stream_started.elapsed().as_millis();
                    if first_event_ms_first_iter.is_none() {
                        first_event_ms_first_iter = Some(t);
                    }
                    first_event_recorded = true;
                }
                match event {
                    StreamEvent::Text(text) => {
                        raw_full_text.push_str(&text);
                        let (visible_text, saw_no_reply, saw_heartbeat) =
                            Self::sanitize_stream_text_chunk(&text);
                        saw_no_reply_token |= saw_no_reply;
                        saw_heartbeat_token |= saw_heartbeat;
                        if !visible_text.is_empty() {
                            self.output.on_text(&visible_text).await;
                            full_text.push_str(&visible_text);
                        }
                    }
                    StreamEvent::ToolCall(call) => {
                        tool_calls.push(call);
                    }
                    StreamEvent::Error(e) => {
                        tracing::error!("LLM Stream Error: {}", e);
                        self.output.on_error(&format!("\n[LLM Error]: {}", e)).await;
                        exit_state = RunExit::HardStop {
                            reason: format!("llm_stream_error: {}", e),
                        };
                        break;
                    }
                    StreamEvent::Done => break,
                }
            }
            llm_stream_wait_ms_acc += llm_stream_started.elapsed().as_millis();

            tracing::debug!("Raw LLM Output (full_text):\n{}", raw_full_text);
            for (i, call) in tool_calls.iter().enumerate() {
                tracing::debug!("Parsed ToolCall [{}]: name={}, args={}", i, call.name, call.args);
            }

            if Self::enforce_final_tag_enabled() {
                if let Some(final_text) = Self::extract_final_tag_content(&full_text) {
                    full_text = final_text;
                } else {
                    if full_text.trim().is_empty() {
                        silent_cause_hint = Some("enforce_final_tag_missing".to_string());
                    } else {
                        tracing::debug!(
                            "CLAW_ENFORCE_FINAL_TAG is enabled, but model response had no <final> tag; keeping visible text as fallback"
                        );
                    }
                }
            }
            if full_text.trim().is_empty() {
                if silent_cause_hint.is_none() && saw_no_reply_token {
                    silent_cause_hint = Some("no_reply_token".to_string());
                }
                if silent_cause_hint.is_none() && saw_heartbeat_token {
                    silent_cause_hint = Some("heartbeat_only".to_string());
                }
                if silent_cause_hint.is_none() && !raw_full_text.trim().is_empty() {
                    silent_cause_hint = Some("control_text_suppressed".to_string());
                }
            }

            // Record assistant message
            let mut parts = Vec::new();
            if !full_text.is_empty() {
                parts.push(Part {
                    text: Some(full_text.clone()),
                    function_call: None,
                    function_response: None,
                });
            }
            if matches!(exit_state, RunExit::HardStop { .. } | RunExit::YieldedToUser) {
                // If we aborted, DO NOT push unexecuted tool calls into the history.
                // It will violate the API protocol (expected function_response next).
                // Just push the partial text if any.
                if let Some(t) = parts.first_mut() {
                    if t.text.is_some() {
                        self.context.add_message_to_current_turn(Message {
                            role: "model".to_string(),
                            parts: vec![t.clone()],
                        });
                    }
                }
                break;
            }

            for call in &tool_calls {
                if call.name.trim().is_empty() {
                    continue; // Skip hallucinated empty tool calls
                }
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

            if !full_text.is_empty() && !full_text.ends_with('\n') {
                self.output.on_text("\n").await;
            }


            if tool_calls.is_empty() {
                
                task_state.last_model_text = full_text.clone();
                
                if task_state.energy_points == 0 {
                    self.output
                        .on_error("[Task] Energy depleted (potential infinite loop). Stopping with partial progress.")
                        .await;
                    exit_state = RunExit::RecoverableFailed {
                        reason: "energy_depleted".to_string(),
                        attempts: task_state.iterations,
                    };
                    break;
                }

                if Self::should_emit_verbose_progress() {
                    self.output
                        .on_text("[Progress] 模型未调用任何工具 (包括 finish_task)。强制要求其继续执行或结案...
")
                        .await;
                }
                
                self.context.add_message_to_current_turn(Message {
                    role: "user".to_string(),
                    parts: vec![Part {
                        text: Some("You did not call any tools. If the task is incomplete, please proceed with your next tool call. If the task is fully completed, you MUST call the `finish_task` tool to exit the loop.".to_string()),
                        function_call: None,
                        function_response: None,
                    }],
                });
                continue;
            }

            

            if full_text.trim().is_empty()
                && tool_calls
                    .iter()
                    .all(|c| Self::is_message_delivery_tool(&c.name))
            {
                silent_cause_hint = Some("message_tool_suppressed_main_reply".to_string());
            }

            // Execute tools
            let mut response_parts = Vec::new();
            let mut requested_finish = false;
            let mut executed_signatures = std::collections::HashSet::new();

            for call in tool_calls {
                let sig = format!("{}:{}", call.name, call.args.to_string());
                if !executed_signatures.insert(sig) {
                    // Deduplicate identical parallel tool calls (common hallucination in Qwen/OpenAI compat)
                    continue;
                }
                let tool_name = call.name.clone();
                let tool_args = call.args.clone();

                // Aliyun/OpenAI compat can sometimes hallucinate empty tool names when it glitches. 
                // We MUST skip these to prevent corrupting the dialogue history which will crash Gemini later.
                if tool_name.trim().is_empty() {
                    continue;
                }

                if tool_name == "finish_task" {
                    requested_finish = true;
                    // Provide explicit summary extraction so the user sees the final answer.
                    let mut display_summary = tool_args.to_string();
                    if let Some(obj) = tool_args.as_object() {
                        if let Some(summary) = obj.get("summary").and_then(|v| v.as_str()) {
                            display_summary = summary.to_string();
                        }
                    }
                    self.output.on_text(&format!("\n\x1b[35m[Agent]: {}\x1b[0m\n", display_summary)).await;
                    break;
                }

                if Self::should_emit_verbose_progress() {
                    self.output
                        .on_text(&format!(
                            "[Progress] 准备执行：{}。目的：{}。
",
                            tool_name,
                            Self::tool_purpose(&tool_name, &tool_args, &full_text)
                        ))
                        .await;
                }

                self.output
                    .on_tool_start(&tool_name, &tool_args.to_string())
                    .await;
                let tool_exec_started = Instant::now();
                let tool_result = self
                    .execute_tool_call_with_recovery(&tool_name, &tool_args, &mut task_state)
                    .await;
                tool_exec_ms_acc += tool_exec_started.elapsed().as_millis();
                if !tool_result.ok {
                    task_state.last_error = Some(tool_result.output.clone());
                }
                let result_str = if tool_result.recovery_attempted {
                    format!(
                        "{}
[Auto-Recovery]
{}",
                        tool_result.output,
                        tool_result.recovery_output.clone().unwrap_or_default()
                    )
                } else {
                    tool_result.output.clone()
                };

                // Dynamic Energy System: Restore energy on successful or meaningful tool usage
                // Cost: 1 energy point per iteration.
                // Reward: +1 for successful commands, +2 for successful file edits/reads. Max cap: 20.
                if tool_result.ok {
                    let reward = if tool_name == "read_file" || tool_name == "write_file" { 2 } else { 1 };
                    task_state.energy_points = (task_state.energy_points + reward).min(25);
                } else {
                    // Penalty: Failed commands don't restore energy. (Already drained 1 at the top of loop).
                    // We can deduct 1 more if we want to punish failures quickly, but maybe that's too aggressive.
                    // Let's keep it forgiving to allow debugging.
                }

                // Deadlock Detection: Has it called the exact same tool with the exact same args multiple times?
                let call_signature = format!("{}:{}", tool_name, tool_args.to_string());
                task_state.recent_tool_signatures.push(call_signature.clone());
                if task_state.recent_tool_signatures.len() > 6 {
                    task_state.recent_tool_signatures.remove(0);
                }

                let mut same_call_count = 0;
                for sig in &task_state.recent_tool_signatures {
                    if sig == &call_signature {
                        same_call_count += 1;
                    }
                }

                if same_call_count >= 3 && !tool_result.ok {
                    self.output.on_error(&format!("\n[Deadlock Detected] Agent has failed with the exact same tool call {} times in a row.\n", same_call_count)).await;
                    
                    // Inject a stern warning into the context
                    let warning_msg = Message {
                        role: "user".to_string(),
                        parts: vec![Part {
                            text: Some(format!(
                                "SYSTEM WARNING: You have executed the exact same failing tool call (`{}`) {} times in a row. You are stuck in a loop. STOP trying this approach immediately. Use `read_file` to review your code, or try a completely different strategy. If you cannot fix it, use `finish_task` to ask the user for help.",
                                tool_name, same_call_count
                            )),
                            function_call: None,
                            function_response: None,
                        }]
                    };
                    self.context.add_message_to_current_turn(warning_msg);
                    
                    // Heavily penalize energy to force exit if it keeps doing it
                    task_state.energy_points = task_state.energy_points.saturating_sub(5);
                }

                self.output.on_tool_end(&result_str).await;

                response_parts.push(Part {
                    text: None,
                    function_call: None,
                    function_response: Some(FunctionResponse {
                        name: tool_name,
                        response: serde_json::json!({ "result": result_str }),
                    }),
                });
            }

            if requested_finish {
                exit_state = RunExit::CompletedWithReply;
                break;
            }

            self.context.add_message_to_current_turn(Message {
                role: "function".to_string(),
                parts: response_parts,
            });
            
            if full_text.trim().is_empty() {
                if let Some(cause) = silent_cause_hint.clone() {
                    exit_state = RunExit::CompletedSilent { cause };
                }
            }
        }

        self.context.end_turn();
        self.emit_recovery_stats(&task_state).await;
        self.emit_timing_report(
            step_started.elapsed().as_millis(),
            retrieval_ms,
            retrieval_timed_out,
            compaction_ms,
            prompt_build_ms_acc,
            llm_setup_ms_acc,
            llm_stream_wait_ms_acc,
            tool_exec_ms_acc,
            task_state.iterations,
            llm_attempts_total,
            first_event_ms_first_iter,
        )
        .await;
        if matches!(
            exit_state,
            RunExit::CompletedSilent { .. }
                if task_state.energy_points == 0
        ) {
            exit_state = RunExit::RecoverableFailed {
                reason: "energy_depleted".to_string(),
                attempts: task_state.iterations,
            };
        } else if matches!(exit_state, RunExit::CompletedSilent { .. })
            && !task_state.last_model_text.trim().is_empty()
        {
            exit_state = RunExit::CompletedWithReply;
        }
        self.output
            .on_text(&format!(
                "[Progress] 任务结束，状态：{}。\n",
                exit_state.label()
            ))
            .await;
        Ok(exit_state)
    }
}

#[cfg(test)]
mod tests {
    use super::AgentLoop;

    #[test]
    fn sanitize_stream_text_chunk_strips_control_tokens() {
        let (text, no_reply, heartbeat) =
            AgentLoop::sanitize_stream_text_chunk("NO_REPLY [HEARTBEAT_OK] visible");
        assert_eq!(text.trim(), "visible");
        assert!(no_reply);
        assert!(heartbeat);
    }

    #[test]
    fn extract_final_tag_content_reads_block() {
        let text = "prefix <final>ship it</final> suffix";
        let extracted = AgentLoop::extract_final_tag_content(text).unwrap();
        assert_eq!(extracted, "ship it");
    }

    #[test]
    fn simple_prompt_uses_two_attempt_budget_for_tool_followup() {
        assert_eq!(AgentLoop::max_attempts_for_input("你叫什么名字？"), AgentLoop::max_task_iterations());
    }

    #[test]
    fn complex_prompt_uses_multi_attempt_budget() {
        assert!(
            AgentLoop::max_attempts_for_input("请修复 src/core.rs 的 bug 并运行 cargo test") > 1
        );
    }
}
