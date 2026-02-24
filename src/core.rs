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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RunExit {
    CompletedWithReply,
    CompletedSilent { cause: String },
    RecoverableFailed { reason: String, attempts: usize },
    HardStop { reason: String },
}

impl RunExit {
    pub fn label(&self) -> &'static str {
        match self {
            RunExit::CompletedWithReply => "completed_with_reply",
            RunExit::CompletedSilent { .. } => "completed_silent",
            RunExit::RecoverableFailed { .. } => "recoverable_failed",
            RunExit::HardStop { .. } => "hard_stop",
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

    pub async fn step(
        &mut self,
        user_input: String,
    ) -> Result<RunExit, Box<dyn std::error::Error + Send + Sync>> {
        // Step 2 & 3: Optimized Retrieval
        // We spawn retrieval as a separate future with a strict timeout.
        // This prevents the bot from hanging for >1s just for RAG.
        let retrieval_future = Self::execute_retrieval_task(
            self.tools.clone(), 
            user_input.clone(), 
            self.output.clone()
        );
        
        // Race: Give it 800ms max. If it takes longer, we skip it for this turn.
        // This ensures the bot feels "snappy" even if the vector DB is slow.
        let (retrieved_text, _sources) = match tokio::time::timeout(Duration::from_millis(800), retrieval_future).await {
            Ok(res) => res,
            Err(_) => {
                // Timeout occurred
                // self.output.on_text("[System] Memory retrieval timed out (skipped for speed).\n").await;
                (String::new(), Vec::new())
            }
        };
        
        // Update context with whatever we got (or empty if timed out)
        self.context.set_retrieved_memory(
            if retrieved_text.is_empty() { None } else { Some(retrieved_text) }, 
            _sources
        );
        let rewritten_query = if self.context.retrieved_memory.is_some() {
             Self::rewrite_memory_query(&user_input)
        } else {
             String::new()
        };

        // Step 1: Async Compaction (Already implemented)
        let compaction_reason = self.maybe_compact_history().await;
        
        self.context.start_turn(user_input);

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
}
