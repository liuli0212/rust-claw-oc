use serde::{Deserialize, Serialize};

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
