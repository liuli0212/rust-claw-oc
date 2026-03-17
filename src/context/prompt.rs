use serde::{Deserialize, Serialize};
use std::fs;

use super::legacy::AgentContext;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DetailedContextStats {
    pub system_static: usize,
    pub system_runtime: usize,
    pub system_custom: usize,
    pub system_project: usize,
    pub system_task_plan: usize,
    pub memory: usize,
    pub history: usize,
    pub current_turn: usize,
    pub last_turn: usize,
    pub total: usize,
    pub max: usize,
    pub truncated_chars: usize,
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

pub(crate) fn build_prompt_sections(ctx: &AgentContext) -> (String, DetailedContextStats) {
    let bpe = AgentContext::get_bpe();
    let mut stats = DetailedContextStats::default();
    let mut sections = Vec::new();

    let identity = ctx.system_prompts.join("\n\n");
    if let Some(section) = build_prompt_section("Identity", identity.clone(), 4_000) {
        stats.system_static = bpe.encode_with_special_tokens(&section).len();
        sections.push(section);
    }

    let mut runtime = String::new();
    runtime.push_str(&format!("OS: {}\n", std::env::consts::OS));
    runtime.push_str(&format!("Architecture: {}\n", std::env::consts::ARCH));
    if let Ok(dir) = std::env::current_dir() {
        runtime.push_str(&format!("Current Directory: {}\n", dir.display()));
    }
    if let Some(path) = &ctx.transcript_path {
        runtime.push_str(&format!("Session Transcript: {}\n", path.display()));
    }
    if let Some(section) = build_prompt_section("Runtime Environment", runtime, 1_000) {
        stats.system_runtime = bpe.encode_with_special_tokens(&section).len();
        sections.push(section);
    }

    if let Ok(custom_prompt) = fs::read_to_string(".claw_prompt.md") {
        if let Some(section) =
            build_prompt_section("User Custom Instructions", custom_prompt, 4_000)
        {
            stats.system_custom = bpe.encode_with_special_tokens(&section).len();
            sections.push(section);
        }
    }

    let mut project_context = String::new();
    if stats.system_task_plan == 0 {
        project_context.push_str("### Task Planning\n");
        project_context.push_str("If the user request is complex (e.g. multi-step refactoring, new feature implementation), you MUST use the `task_plan` tool immediately to create a structured plan (action='add').\n\n");
    }
    if let Ok(content) = fs::read_to_string("AGENTS.md") {
        project_context.push_str("### AGENTS.md\n");
        project_context.push_str(&AgentContext::truncate_chars(&content, 3_000));
        project_context.push_str("\n\n");
    }
    if let Ok(content) = fs::read_to_string("MEMORY.md") {
        project_context.push_str("### MEMORY.md\n");
        project_context.push_str(&AgentContext::truncate_chars(&content, 1_500));
        project_context.push_str("\n\n");
    }
    if let Some(section) = build_prompt_section("Project Context", project_context, 7_000) {
        stats.system_project = bpe.encode_with_special_tokens(&section).len();
        sections.push(section);
    }

    if let Some(memory) = &ctx.retrieved_memory {
        if let Some(section) = build_prompt_section("Retrieved Memory", memory.clone(), 3_000) {
            stats.memory = bpe.encode_with_special_tokens(&section).len();
            sections.push(section);
        }
    }

    stats.max = ctx.max_history_tokens;
    (sections.join("\n"), stats)
}

pub(crate) fn get_detailed_stats(
    ctx: &AgentContext,
    pending_user_input: Option<&str>,
) -> DetailedContextStats {
    let (_, mut stats) = build_prompt_sections(ctx);
    let bpe = AgentContext::get_bpe();

    let (_, history_tokens, _, truncated_chars) = ctx.build_history_with_budget();
    stats.history = history_tokens;
    stats.truncated_chars = truncated_chars;

    if let Some(turn) = &ctx.current_turn {
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

    if let Some(last) = ctx.dialogue_history.last() {
        stats.last_turn = AgentContext::turn_token_estimate(last, &bpe);
    }

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

pub(crate) fn build_system_prompt(ctx: &AgentContext) -> String {
    let (prompt, _) = build_prompt_sections(ctx);
    prompt
}

fn build_prompt_section(title: &str, content: String, max_chars: usize) -> Option<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    let truncated = AgentContext::truncate_chars(trimmed, max_chars);
    Some(format!("## {title}\n{truncated}\n"))
}
