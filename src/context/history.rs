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
