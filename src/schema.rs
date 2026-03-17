use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Correlation identifiers for tracking execution flow
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrelationIds {
    pub session_id: String,
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    pub event_id: Option<String>,
}

pub struct StoragePaths;

impl StoragePaths {
    pub fn session_dir(session_id: &str) -> PathBuf {
        PathBuf::from("rusty_claw")
            .join("sessions")
            .join(session_id)
    }

    #[cfg(test)]
    pub fn events_file(session_id: &str) -> PathBuf {
        Self::session_dir(session_id).join("events.jsonl")
    }

    pub fn task_state_file(session_id: &str) -> PathBuf {
        Self::session_dir(session_id).join("task_state.json")
    }

    #[cfg(test)]
    pub fn artifacts_dir(session_id: &str, run_id: &str) -> PathBuf {
        PathBuf::from("rusty_claw")
            .join("artifacts")
            .join(session_id)
            .join(run_id)
    }

    #[cfg(test)]
    pub fn session_transcript_file(session_id: &str) -> PathBuf {
        Self::session_dir(session_id).join("transcript.json")
    }
}
