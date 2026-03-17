use super::history::ContextDiff;
use super::legacy::AgentContext;

pub fn format_context_details(ctx: &AgentContext) -> String {
    let stats = ctx.get_detailed_stats(None);

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
        if let Ok(meta) = std::fs::metadata(file) {
            details.push_str(&format!("    * {} ({} bytes)\n", file, meta.len()));
        }
    }

    details.push_str("\n\x1b[1;33m[Conversation History]\x1b[0m\n");
    let (_, _, turns_included, _) = ctx.build_history_with_budget();
    details.push_str(&format!(
        "  - History Load:       {} tokens ({} turns included)\n",
        stats.history, turns_included
    ));
    details.push_str(&format!(
        "  - Total History:      {} tokens ({} turns total)\n",
        ctx.dialogue_history_token_estimate(),
        ctx.dialogue_history.len()
    ));

    if stats.memory > 0 {
        details.push_str("\n\x1b[1;33m[RAG Memory]\x1b[0m\n");
        details.push_str(&format!("  - Retrieved:          {} tokens\n", stats.memory));
        for src in &ctx.retrieved_memory_sources {
            details.push_str(&format!("    * {}\n", src));
        }
    }

    if let Some(turn) = &ctx.current_turn {
        details.push_str("\n\x1b[1;33m[Current Turn]\x1b[0m\n");
        details.push_str(&format!(
            "  - Active Payload:     {} tokens\n",
            stats.current_turn
        ));
        details.push_str(&format!(
            "  - User Message:       {}\n",
            AgentContext::truncate_chars(&turn.user_message, 80)
        ));
    }

    details
}

pub fn format_context_diff(diff: &ContextDiff) -> String {
    let mut output = String::new();
    output.push_str("\n\x1b[1;36m=== Context Diff ===\x1b[0m\n");

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

    let turn_sign = if diff.history_turns_delta >= 0 { "+" } else { "" };
    output.push_str(&format!(
        "  History:      {}{}\x1b[0m turns\n",
        turn_sign, diff.history_turns_delta
    ));

    if diff.system_prompt_changed {
        output.push_str("  System:       \x1b[33mCHANGED\x1b[0m\n");
    } else {
        output.push_str("  System:       Unchanged\n");
    }

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

pub fn inspect_context_section(ctx: &AgentContext, section: &str, arg: Option<&str>) -> String {
    match section {
        "system" => ctx.build_system_prompt(),
        "history" => {
            let count = arg.and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
            let start = ctx.dialogue_history.len().saturating_sub(count);
            let mut output = String::new();
            for (i, turn) in ctx.dialogue_history.iter().enumerate().skip(start) {
                output.push_str(&format!(
                    "\n\x1b[1;33m[Turn {} - {}]\x1b[0m\n",
                    i + 1,
                    turn.turn_id
                ));
                output.push_str(&format!("User: {}\n", turn.user_message));
                output.push_str(&format!("Messages: {}\n", turn.messages.len()));
            }
            if let Some(current) = &ctx.current_turn {
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
            if let Some(mem) = &ctx.retrieved_memory {
                format!("Sources: {:?}\n\n{}", ctx.retrieved_memory_sources, mem)
            } else {
                "No memory retrieved.".to_string()
            }
        }
        _ => format!("Unknown section: {}", section),
    }
}
