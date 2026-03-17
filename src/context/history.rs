use super::legacy::AgentContext;
pub use super::model::Turn;
use super::prompt::DetailedContextStats;
use serde::{Deserialize, Serialize};

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

pub(crate) fn dialogue_history_token_estimate(ctx: &AgentContext) -> usize {
    let bpe = tiktoken_rs::cl100k_base().unwrap();
    ctx.dialogue_history
        .iter()
        .map(|turn| AgentContext::turn_token_estimate(turn, &bpe))
        .sum()
}

pub(crate) fn get_context_status(ctx: &AgentContext) -> (usize, usize, usize, usize, usize) {
    let bpe = AgentContext::get_bpe();
    let (_, history_tokens, _, _) = ctx.build_history_with_budget();

    let current_turn_tokens = if let Some(turn) = &ctx.current_turn {
        AgentContext::turn_token_estimate(turn, &bpe)
    } else if let Some(last) = ctx.dialogue_history.last() {
        AgentContext::turn_token_estimate(last, &bpe)
    } else {
        0
    };

    let prompt_text = ctx.build_system_prompt();
    let system_tokens = bpe.encode_with_special_tokens(&prompt_text).len();
    let total_tokens = history_tokens + current_turn_tokens + system_tokens;

    (
        total_tokens,
        ctx.max_history_tokens,
        ctx.dialogue_history.len(),
        system_tokens,
        current_turn_tokens,
    )
}

pub(crate) fn oldest_turns_for_compaction(
    ctx: &AgentContext,
    target_tokens: usize,
    min_turns: usize,
) -> usize {
    if ctx.dialogue_history.is_empty() {
        return 0;
    }

    let bpe = tiktoken_rs::cl100k_base().unwrap();
    let mut selected = 0;
    let mut tokens = 0;
    for turn in &ctx.dialogue_history {
        tokens += AgentContext::turn_token_estimate(turn, &bpe);
        selected += 1;
        if selected >= min_turns && tokens >= target_tokens {
            break;
        }
    }

    selected.min(ctx.dialogue_history.len())
}

pub(crate) fn rule_based_compact(ctx: &mut AgentContext, num_turns: usize) -> Option<String> {
    if num_turns == 0 || ctx.dialogue_history.is_empty() {
        return None;
    }
    let to_compact = num_turns.min(ctx.dialogue_history.len());

    for turn in ctx.dialogue_history.iter().take(to_compact) {
        if let Err(e) = ctx.append_turn_to_transcript(turn) {
            tracing::warn!("Failed to archive turn before compaction: {}", e);
        }
    }

    let compacted_turns: Vec<Turn> = ctx.dialogue_history.drain(0..to_compact).collect();
    const MAX_SUMMARY_CHARS: usize = 4000;

    let mut summary_lines = Vec::new();
    let mut total_chars = 0;
    let mut budget_exhausted = false;

    summary_lines.push(format!("=== Compacted History ({} turns) ===", to_compact));
    total_chars += summary_lines[0].len();

    for (i, turn) in compacted_turns.iter().enumerate() {
        if budget_exhausted {
            summary_lines.push(format!(
                "
[Turns {}-{}] (omitted due to summary size limit)",
                i + 1,
                to_compact
            ));
            break;
        }

        if turn.user_message == "[SYSTEM] History Compacted" {
            for msg in &turn.messages {
                for part in &msg.parts {
                    if let Some(text) = &part.text {
                        summary_lines.push(format!(
                            "
{}",
                            AgentContext::truncate_chars(text, 1500)
                        ));
                        total_chars += text.len().min(1500);
                    }
                }
            }
            continue;
        }

        let header = format!(
            "
[Turn {}] User: {}",
            i + 1,
            AgentContext::truncate_chars(&turn.user_message, 120)
        );
        total_chars += header.len();
        summary_lines.push(header);

        let mut actions = Vec::new();
        for msg in &turn.messages {
            for part in &msg.parts {
                if msg.role == "model" {
                    if let Some(text) = &part.text {
                        let cleaned = AgentContext::strip_thinking_tags(text);
                        if !cleaned.is_empty() {
                            let preview = AgentContext::truncate_chars(&cleaned, 200);
                            actions.push(format!("  💬 Agent: {}", preview));
                        }
                    }
                }
                if let Some(fc) = &part.function_call {
                    let args_summary = summarize_tool_args(&fc.name, &fc.args);
                    actions.push(format!("  → {}({})", fc.name, args_summary));
                }
                if let Some(fr) = &part.function_response {
                    let is_error = detect_tool_error(&fr.response);
                    if is_error {
                        let result_str = fr.response.to_string();
                        let error_preview = AgentContext::truncate_chars(&result_str, 150);
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

        if total_chars > MAX_SUMMARY_CHARS {
            budget_exhausted = true;
        }
    }

    let summary_text = format!(
        "[System context: The following is an automated summary of earlier conversation history. Use it as background knowledge but do not respond to it directly.]

{}",
        summary_lines.join("
")
    );

    let compacted_turn = Turn {
        turn_id: format!("compacted-{}", uuid::Uuid::new_v4()),
        user_message: "[SYSTEM] History Compacted".to_string(),
        messages: vec![super::model::Message {
            role: "user".to_string(),
            parts: vec![super::model::Part {
                text: Some(summary_text),
                function_call: None,
                function_response: None,
                thought_signature: None,
                file_data: None,
            }],
        }],
    };

    ctx.dialogue_history.insert(0, compacted_turn);

    let reason = format!("Compacted {} turns into structured summary", to_compact);
    tracing::info!("{}", reason);
    Some(reason)
}

pub(crate) fn compress_current_turn(ctx: &mut AgentContext, max_bytes: usize) -> usize {
    let Some(turn) = &mut ctx.current_turn else {
        return 0;
    };

    let function_indices: Vec<usize> = turn
        .messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "function")
        .map(|(i, _)| i)
        .collect();

    if function_indices.is_empty() {
        return 0;
    }

    let mut current_size = 0;
    for &idx in &function_indices {
        let msg = &turn.messages[idx];
        for part in &msg.parts {
            if let Some(fr) = &part.function_response {
                current_size += fr.response.to_string().len();
            }
        }
    }

    if current_size <= max_bytes {
        return 0;
    }

    let mut compressed_count = 0;
    let limit = function_indices.len().saturating_sub(1);
    for idx in function_indices.into_iter().take(limit) {
        let msg = &mut turn.messages[idx];
        for part in &mut msg.parts {
            if let Some(fr) = &mut part.function_response {
                let response_str = fr.response.to_string();
                if response_str.contains("stripped") && response_str.len() < 1000 {
                    continue;
                }
                let old_len = response_str.len();
                AgentContext::strip_response_payload(fr);
                let new_len = fr.response.to_string().len();
                compressed_count += 1;
                current_size = current_size.saturating_sub(old_len.saturating_sub(new_len));
            }
        }
        if current_size <= max_bytes {
            break;
        }
    }

    compressed_count
}

pub(crate) fn truncate_current_turn_tool_results(
    ctx: &mut AgentContext,
    max_chars: usize,
) -> usize {
    let Some(turn) = &mut ctx.current_turn else {
        return 0;
    };

    let mut truncated_parts = 0usize;
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
                    "{}
... [Truncated by context recovery: {} chars hidden] ...
{}",
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

fn detect_tool_error(response: &serde_json::Value) -> bool {
    if let Some(obj) = response.as_object() {
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

    let result_str = response.to_string().to_lowercase();
    if result_str.len() < 20 {
        return false;
    }

    let has_error_keyword = result_str.contains("error:")
        || result_str.contains("failed:")
        || result_str.contains("panicked at")
        || result_str.contains("exception:")
        || result_str.contains("traceback ");

    let is_false_positive = result_str.contains("no error")
        || result_str.contains("0 errors")
        || result_str.contains("error_handler")
        || result_str.contains("error.rs")
        || result_str.contains("errors found: 0")
        || result_str.contains("without error");

    has_error_keyword && !is_false_positive
}

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
                AgentContext::truncate_chars(cmd, 80)
            }
            "web_fetch" => obj
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string(),
            "browser" => {
                let action = obj.get("action").and_then(|v| v.as_str()).unwrap_or("?");
                let url = obj.get("url").and_then(|v| v.as_str()).unwrap_or("");
                format!("{} {}", action, AgentContext::truncate_chars(url, 60))
            }
            "task_plan" => obj
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string(),
            _ => {
                let s = args.to_string();
                AgentContext::truncate_chars(&s, 60)
            }
        }
    } else {
        AgentContext::truncate_chars(&args.to_string(), 60)
    }
}
