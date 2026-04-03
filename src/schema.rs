use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Correlation identifiers for tracking execution flow
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrelationIds {
    pub session_id: String,
    pub task_id: Option<String>,
    pub run_id: Option<String>,
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

    pub fn events_file(session_id: &str) -> PathBuf {
        Self::session_dir(session_id).join("events.jsonl")
    }

    pub fn task_state_file(session_id: &str) -> PathBuf {
        Self::session_dir(session_id).join("task_state.json")
    }

    pub fn trace_root_dir() -> PathBuf {
        PathBuf::from("rusty_claw").join("trace_center")
    }

    pub fn trace_records_dir() -> PathBuf {
        Self::trace_root_dir().join("records")
    }

    pub fn trace_runs_dir() -> PathBuf {
        Self::trace_root_dir().join("runs")
    }

    pub fn trace_index_file() -> PathBuf {
        Self::trace_root_dir().join("index.sqlite")
    }

    pub fn trace_run_records_file(run_id: &str) -> PathBuf {
        Self::trace_records_dir().join(format!("{}.jsonl", run_id))
    }

    pub fn trace_run_summary_file(run_id: &str) -> PathBuf {
        Self::trace_runs_dir().join(format!("{}.json", run_id))
    }

    #[allow(dead_code)]
    pub fn artifacts_dir(session_id: &str, run_id: &str) -> PathBuf {
        PathBuf::from("rusty_claw")
            .join("artifacts")
            .join(session_id)
            .join(run_id)
    }

    pub fn session_transcript_file(session_id: &str) -> PathBuf {
        Self::session_dir(session_id).join("transcript.json")
    }
}
