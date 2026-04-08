use super::agent_context::AgentContext;
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

fn summarize_tool_args(_tool_name: &str, args: &serde_json::Value) -> String {
    if let Some(obj) = args.as_object() {
        if let Some(action) = obj.get("action").and_then(|v| v.as_str()) {
            let target = obj
                .get("target_url")
                .or_else(|| obj.get("url"))
                .and_then(|v| v.as_str())
                .map(|value| AgentContext::truncate_chars(value, 60))
                .unwrap_or_default();
            return if target.is_empty() {
                action.to_string()
            } else {
                format!("{} {}", action, target)
            };
        }

        if let Some(path) = obj.get("path").and_then(|v| v.as_str()) {
            return path.to_string();
        }

        if let Some(cmd) = obj.get("command").and_then(|v| v.as_str()) {
            return AgentContext::truncate_chars(cmd, 80);
        }

        if let Some(url) = obj.get("url").and_then(|v| v.as_str()) {
            return url.to_string();
        }

        let s = args.to_string();
        AgentContext::truncate_chars(&s, 60)
    } else {
        AgentContext::truncate_chars(&args.to_string(), 60)
    }
}

fn compact_function_call_args_for_history(
    _tool_name: &str,
    args: &serde_json::Value,
) -> Option<serde_json::Value> {
    let obj = args.as_object()?;

    if let Some(action) = obj.get("action").and_then(|v| v.as_str()) {
        let mut compact = serde_json::Map::new();
        compact.insert(
            "action".to_string(),
            serde_json::Value::String(action.to_string()),
        );

        if let Some(target_url) = obj.get("target_url").and_then(|v| v.as_str()) {
            compact.insert(
                "target_url".to_string(),
                serde_json::Value::String(AgentContext::truncate_chars(target_url, 60)),
            );
        } else if let Some(url) = obj.get("url").and_then(|v| v.as_str()) {
            compact.insert(
                "url".to_string(),
                serde_json::Value::String(AgentContext::truncate_chars(url, 60)),
            );
        }

        return Some(serde_json::Value::Object(compact));
    }

    if let Some(path) = obj.get("path").and_then(|v| v.as_str()) {
        return Some(serde_json::json!({ "path": path }));
    }

    if let Some(command) = obj.get("command").and_then(|v| v.as_str()) {
        return Some(serde_json::json!({
            "command": AgentContext::truncate_chars(command, 80)
        }));
    }

    if let Some(url) = obj.get("url").and_then(|v| v.as_str()) {
        return Some(serde_json::json!({ "url": url }));
    }

    None
}

pub(crate) fn sanitize_turn(turn: &Turn) -> Option<Turn> {
    let mut messages = Vec::new();
    for msg in &turn.messages {
        if let Some(cleaned) = sanitize_message(msg) {
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

pub(crate) fn build_history_with_budget(
    ctx: &AgentContext,
) -> (Vec<super::model::Message>, usize, usize, usize) {
    let bpe = tiktoken_rs::cl100k_base().unwrap();
    let history_budget = ctx.max_history_tokens.saturating_mul(85) / 100;
    let mut history_blocks: Vec<(usize, Vec<super::model::Message>)> = Vec::new();
    let mut current_tokens = 0;
    let mut turns_included = 0;
    let mut total_truncated_chars = 0;
    let mut protect_next_turn = false;

    for (i, turn) in ctx.dialogue_history.iter().rev().enumerate() {
        let sanitized = match sanitize_turn(turn) {
            Some(v) => v,
            None => continue,
        };

        let user_asks_for_context = i < 10 && is_user_referencing_history(&turn.user_message);
        let should_strip = i >= 3 && !protect_next_turn;

        let (turn, truncated) = if should_strip {
            reconstruct_turn_for_history(&sanitized)
        } else {
            truncate_old_tool_results(&sanitized)
        };
        total_truncated_chars += truncated;
        protect_next_turn = user_asks_for_context;

        let turn_tokens: usize = turn
            .messages
            .iter()
            .map(|m| AgentContext::estimate_tokens(&bpe, m))
            .sum();

        if current_tokens + turn_tokens > history_budget {
            break;
        }
        current_tokens += turn_tokens;
        history_blocks.push((i, turn.messages));
        turns_included += 1;
    }

    history_blocks.reverse();
    let mut flattened = Vec::new();
    let mut prev_zone: Option<u8> = None;

    for (distance, block) in &history_blocks {
        let zone = if *distance >= 10 {
            0u8
        } else if *distance >= 3 {
            1u8
        } else {
            2u8
        };
        if prev_zone.is_none() || prev_zone != Some(zone) {
            let label = match zone {
                0 => Some("--- [EARLIER HISTORY] ---"),
                1 => Some("--- [RECENT CONTEXT] ---"),
                _ => None,
            };
            if let Some(label_text) = label {
                flattened.push(super::model::Message {
                    role: "user".to_string(),
                    parts: vec![super::model::Part {
                        text: Some(label_text.to_string()),
                        function_call: None,
                        function_response: None,
                        thought_signature: None,
                        file_data: None,
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

fn sanitize_message(msg: &super::model::Message) -> Option<super::model::Message> {
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
        if role == "user" {
            cleaned_parts.push(super::model::Part {
                text: Some("[Acknowledged]".to_string()),
                function_call: None,
                function_response: None,
                thought_signature: None,
                file_data: None,
            });
        } else if role == "model" {
            cleaned_parts.push(super::model::Part {
                text: Some("[Thought process hidden]".to_string()),
                function_call: None,
                function_response: None,
                thought_signature: None,
                file_data: None,
            });
        } else {
            return None;
        }
    }

    Some(super::model::Message {
        role: msg.role.clone(),
        parts: cleaned_parts,
    })
}

fn truncate_old_tool_results(turn: &Turn) -> (Turn, usize) {
    let mut cloned = turn.clone();
    let mut total_chars_hidden = 0;
    for msg in &mut cloned.messages {
        for part in &mut msg.parts {
            part.thought_signature = None;
            if let Some(fr) = &mut part.function_response {
                total_chars_hidden += truncate_function_response(fr);
            }
        }
    }
    (cloned, total_chars_hidden)
}

fn truncate_function_response(fr: &mut super::model::FunctionResponse) -> usize {
    const MAX_RESULT_LEN: usize = 12_000;
    const MAX_TOTAL_LEN: usize = 20_000;

    // 1. Try to truncate "result" field if it's an object response
    if let Some(obj) = fr.response.as_object_mut() {
        if let Some(val) = obj.get_mut("result") {
            if let Some(truncated) = truncate_large_json_value(val, MAX_RESULT_LEN) {
                return truncated;
            }
        }
    }

    // 2. Fallback: truncate the entire response if it's still too large
    let response_str = fr.response.to_string();
    if response_str.len() > MAX_TOTAL_LEN {
        let original_len = response_str.len();
        let char_count = response_str.chars().count();
        let head: String = response_str.chars().take(2_000).collect();
        fr.response = serde_json::json!({
            "result": format!("{}\n... [Truncated massive object] ...", head),
            "original_chars": original_len
        });
        return char_count.saturating_sub(2000); // Approximate hidden count
    }

    0
}

fn truncate_large_json_value(val: &mut serde_json::Value, max_len: usize) -> Option<usize> {
    let s = if let Some(s) = val.as_str() {
        s.to_string()
    } else {
        let serialized = val.to_string();
        if serialized.len() <= max_len {
            return None;
        }
        serialized
    };

    let char_count = s.chars().count();
    if char_count <= max_len {
        return None;
    }

    let is_string = val.is_string();
    let (truncated_text, truncated_count) = if is_string {
        let keep = max_len / 2;
        let head: String = s.chars().take(keep).collect();
        let tail: String = s.chars().skip(char_count - keep).collect();
        (
            format!(
                "{}\n... [History Compressed: {} chars hidden] ...\n{}",
                head,
                char_count.saturating_sub(max_len),
                tail
            ),
            char_count.saturating_sub(max_len),
        )
    } else {
        let head: String = s.chars().take(4_000).collect();
        (
            format!("{}\n... [History Object Compressed] ...", head),
            char_count.saturating_sub(4_000),
        )
    };

    *val = serde_json::Value::String(truncated_text);
    Some(truncated_count)
}

fn is_user_referencing_history(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    let keywords = [
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

fn reconstruct_turn_for_history(turn: &Turn) -> (Turn, usize) {
    let mut new_messages = Vec::new();
    for msg in &turn.messages {
        let mut new_parts = Vec::new();
        for part in &msg.parts {
            let mut new_part = super::model::Part {
                text: None,
                function_call: None,
                function_response: None,
                thought_signature: None,
                file_data: None,
            };
            if let Some(fc) = &part.function_call {
                let mut stripped_fc = fc.clone();
                if let Some(compact_args) =
                    compact_function_call_args_for_history(&fc.name, &fc.args)
                {
                    stripped_fc.args = compact_args;
                }
                new_part.function_call = Some(stripped_fc);
            }
            if let Some(fr) = &part.function_response {
                let mut stripped_fr = fr.clone();
                AgentContext::strip_response_payload(&mut stripped_fr);
                new_part.function_response = Some(stripped_fr);
            }
            if let Some(text) = &part.text {
                if msg.role == "user" {
                    let mut cleaned_text = text.clone();
                    for marker in [
                        "[CURRENT TASK]",
                        "--- [RECENT CONTEXT] ---",
                        "--- [EARLIER HISTORY] ---",
                    ] {
                        if cleaned_text.contains(marker) {
                            cleaned_text = cleaned_text.replace(marker, "").trim().to_string();
                        }
                    }
                    if cleaned_text.is_empty() {
                        cleaned_text = "[Acknowledged]".to_string();
                    }
                    new_part.text = Some(cleaned_text);
                } else if msg.role == "model" && new_part.function_call.is_none() {
                    let cleaned = AgentContext::strip_thinking_tags(text);
                    if !cleaned.is_empty() {
                        new_part.text = Some(cleaned);
                    } else {
                        new_part.text = Some("[Thought process hidden]".to_string());
                    }
                }
            }
            if new_part.text.is_some()
                || new_part.function_call.is_some()
                || new_part.function_response.is_some()
            {
                new_parts.push(new_part);
            }
        }
        if !new_parts.is_empty() {
            new_messages.push(super::model::Message {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::model::{FunctionCall, Message, Part};

    #[test]
    fn reconstruct_turn_compacts_task_plan_and_path_based_calls() {
        let turn = Turn {
            turn_id: "turn-1".to_string(),
            user_message: "do work".to_string(),
            messages: vec![Message {
                role: "model".to_string(),
                parts: vec![
                    Part {
                        text: None,
                        function_call: Some(FunctionCall {
                            name: "task_plan".to_string(),
                            args: serde_json::json!({
                                "action": "update_status",
                                "index": 1,
                                "status": "completed"
                            }),
                            id: None,
                        }),
                        function_response: None,
                        thought_signature: None,
                        file_data: None,
                    },
                    Part {
                        text: None,
                        function_call: Some(FunctionCall {
                            name: "read_file".to_string(),
                            args: serde_json::json!({
                                "path": "/tmp/demo.txt",
                                "thought": "inspect"
                            }),
                            id: None,
                        }),
                        function_response: None,
                        thought_signature: None,
                        file_data: None,
                    },
                ],
            }],
        };

        let (rebuilt, _) = reconstruct_turn_for_history(&turn);
        let parts = &rebuilt.messages[0].parts;

        assert_eq!(
            parts[0].function_call.as_ref().unwrap().args,
            serde_json::json!({ "action": "update_status" })
        );
        assert_eq!(
            parts[1].function_call.as_ref().unwrap().args,
            serde_json::json!({ "path": "/tmp/demo.txt" })
        );
    }

    #[test]
    fn test_truncate_function_response_string() {
        use crate::context::model::FunctionResponse;
        let mut fr = FunctionResponse {
            id: None,
            name: "test_tool".to_string(),
            response: serde_json::json!({
                "result": "A".repeat(15_000)
            }),
        };
        let truncated = truncate_function_response(&mut fr);
        assert!(truncated > 0);
        let result_str = fr.response["result"].as_str().unwrap();
        assert!(result_str.contains("History Compressed"));
        assert!(result_str.len() < 15_000);
    }

    #[test]
    fn test_truncate_function_response_obj() {
        use crate::context::model::FunctionResponse;
        let mut fr = FunctionResponse {
            id: None,
            name: "test_tool".to_string(),
            response: serde_json::json!({
                "result": { "large_data": "A".repeat(15_000) }
            }),
        };
        let truncated = truncate_function_response(&mut fr);
        assert!(truncated > 0);
        let result_str = fr.response["result"].as_str().unwrap();
        assert!(result_str.contains("History Object Compressed"));
    }
}
