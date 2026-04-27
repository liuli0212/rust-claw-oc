use serde::{Deserialize, Serialize};
use std::fs;

use super::agent_context::AgentContext;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

struct WorkspacePromptParts {
    project_context: String,
    durable_memory: Option<String>,
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

    let workspace = build_workspace_prompt_parts(stats.system_task_plan == 0);
    let project_context = workspace.project_context_with_inline_memory();
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

fn build_workspace_prompt_parts(include_task_planning: bool) -> WorkspacePromptParts {
    let mut project_context = String::new();

    if include_task_planning {
        project_context.push_str("### Task Planning\n");
        project_context.push_str("If the user request is complex (e.g. multi-step refactoring, new feature implementation), you MUST use the `task_plan` tool immediately to create a structured plan (action='add').\n\n");
    }

    if let Ok(content) = fs::read_to_string("AGENTS.md") {
        project_context.push_str("### AGENTS.md\n");
        project_context.push_str(&AgentContext::truncate_chars(&content, 3_000));
        project_context.push_str("\n\n");
    }

    WorkspacePromptParts {
        project_context,
        durable_memory: fs::read_to_string("MEMORY.md").ok(),
    }
}

impl WorkspacePromptParts {
    fn project_context_with_inline_memory(&self) -> String {
        let mut project_context = self.project_context.clone();
        if let Some(content) = &self.durable_memory {
            project_context.push_str("### MEMORY.md\n");
            project_context.push_str(&AgentContext::truncate_chars(content, 1_500));
            project_context.push_str("\n\n");
        }
        project_context
    }
}

pub(crate) fn build_llm_payload(
    ctx: &AgentContext,
    task_state: &crate::task_state::TaskStateSnapshot,
    _assembler: &crate::context_assembler::ContextAssembler,
) -> (
    Vec<super::model::Message>,
    Option<super::model::Message>,
    PromptReport,
) {
    let bpe = tiktoken_rs::cl100k_base().unwrap();
    let mut current_turn_messages = Vec::new();
    let mut current_turn_tokens = 0;
    if let Some(turn) = &ctx.current_turn {
        if let Some(sanitized_turn) = super::history::sanitize_turn(turn) {
            let separator = super::model::Message {
                role: "user".to_string(),
                parts: vec![super::model::Part {
                    text: Some("--- [CURRENT TASK] ---".to_string()),
                    function_call: None,
                    function_response: None,
                    thought_signature: None,
                    file_data: None,
                }],
            };
            current_turn_tokens += AgentContext::estimate_tokens(&bpe, &separator);
            current_turn_messages.push(separator);

            current_turn_tokens += sanitized_turn
                .messages
                .iter()
                .map(|m| AgentContext::estimate_tokens(&bpe, m))
                .sum::<usize>();
            current_turn_messages.extend(sanitized_turn.messages);
        }
    }

    let mut system_static = Vec::new();
    system_static.push(ctx.system_prompts.join("\n\n"));

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

    let workspace = build_workspace_prompt_parts(false);
    system_static.push(format!("## Project Context\n{}", workspace.project_context));

    let mut active_evidence = ctx.active_evidence.clone();
    if let Some(mem) = &ctx.retrieved_memory {
        active_evidence.push(crate::evidence::Evidence::new(
            "legacy_rag".into(),
            "memory".into(),
            "retrieved".into(),
            1.0,
            "Retrieved memory snippets".into(),
            mem.clone(),
        ));
    }

    let system_budget = ctx
        .max_history_tokens
        .saturating_sub(current_turn_tokens)
        .saturating_sub(super::history::response_token_reserve(
            ctx.max_history_tokens,
        ));
    let system_assembler = crate::context_assembler::ContextAssembler::new(system_budget);
    let (assembled_system_text, report_data) = system_assembler.assemble_prompt(
        &system_static.join("\n\n"),
        "",
        workspace.durable_memory.as_deref(),
        ctx.skill_contract.as_deref(),
        ctx.skill_instructions.as_deref(),
        ctx.skill_state_summary.as_deref(),
        ctx.execution_notices.as_deref(),
        task_state,
        active_evidence,
        Vec::new(),
    );
    let final_system_text = assembled_system_text;

    let system_msg = super::model::Message {
        role: "system".to_string(),
        parts: vec![super::model::Part {
            thought_signature: None,
            text: Some(final_system_text),
            function_call: None,
            function_response: None,
            file_data: None,
        }],
    };

    let system_prompt_tokens = report_data.used_tokens;
    let history_budget = super::history::effective_history_budget(
        ctx.max_history_tokens,
        system_prompt_tokens,
        current_turn_tokens,
    );
    let (mut messages, history_tokens_used, history_turns_included, _) =
        ctx.build_history_with_token_budget(history_budget);
    messages.extend(current_turn_messages);
    let retrieved_memory_snippets = ctx.retrieved_memory_sources.len();

    let report = PromptReport {
        max_history_tokens: ctx.max_history_tokens,
        history_tokens_used,
        history_turns_included,
        current_turn_tokens,
        system_prompt_tokens,
        total_prompt_tokens: history_tokens_used + current_turn_tokens + system_prompt_tokens,
        retrieved_memory_snippets,
        retrieved_memory_sources: ctx.retrieved_memory_sources.clone(),
        detailed_stats: ctx.get_detailed_stats(None),
    };

    (messages, Some(system_msg), report)
}
