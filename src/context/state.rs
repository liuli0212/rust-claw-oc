use super::agent_context::AgentContext;
use super::history::{ContextDiff, ContextSnapshot};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

pub fn take_snapshot(ctx: &mut AgentContext) -> ContextSnapshot {
    let stats = ctx.get_detailed_stats(None);
    let system_prompt = ctx.build_system_prompt();
    let mut hasher = DefaultHasher::new();
    system_prompt.hash(&mut hasher);
    let hash = hasher.finish();

    let snapshot = ContextSnapshot {
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        turn_id: ctx
            .current_turn
            .as_ref()
            .map(|t| t.turn_id.clone())
            .unwrap_or_default(),
        stats,
        messages_count: ctx
            .dialogue_history
            .iter()
            .map(|t| t.messages.len())
            .sum::<usize>()
            + ctx
                .current_turn
                .as_ref()
                .map(|t| t.messages.len())
                .unwrap_or(0),
        system_prompt_hash: hash,
        retrieved_memory_sources: ctx.retrieved_memory_sources.clone(),
        history_turns_count: ctx.dialogue_history.len(),
    };
    ctx.last_snapshot = Some(snapshot.clone());
    snapshot
}

pub fn diff_snapshot(ctx: &AgentContext, old: &ContextSnapshot) -> ContextDiff {
    let current_stats = ctx.get_detailed_stats(None);
    let system_prompt = ctx.build_system_prompt();
    let mut hasher = DefaultHasher::new();
    system_prompt.hash(&mut hasher);
    let current_hash = hasher.finish();

    let old_sources: std::collections::HashSet<_> =
        old.retrieved_memory_sources.iter().cloned().collect();
    let new_sources_set: std::collections::HashSet<_> =
        ctx.retrieved_memory_sources.iter().cloned().collect();

    let new_sources = ctx
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
        history_turns_delta: ctx.dialogue_history.len() as i32 - old.history_turns_count as i32,
        system_prompt_changed: current_hash != old.system_prompt_hash,
        new_sources,
        removed_sources,
        memory_changed: ctx.retrieved_memory_sources != old.retrieved_memory_sources,
        truncated_delta: current_stats.truncated_chars as i64 - old.stats.truncated_chars as i64,
    }
}
