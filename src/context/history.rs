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
