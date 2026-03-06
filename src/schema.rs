use std::path::{Path, PathBuf};

use uuid::Uuid;

pub const EVENT_LOG_SCHEMA_VERSION: u32 = 1;
pub const TASK_STATE_SCHEMA_VERSION: u32 = 1;
pub const EVIDENCE_SCHEMA_VERSION: u32 = 1;
pub const ARTIFACT_SCHEMA_VERSION: u32 = 1;

pub const RUSTY_CLAW_DIR: &str = "rusty_claw";
pub const SESSIONS_DIR: &str = "sessions";
pub const ARTIFACTS_DIR: &str = "artifacts";
pub const SESSIONS_REGISTRY_FILE: &str = "sessions.json";
pub const EVENT_LOG_FILE: &str = "events.jsonl";
pub const TASK_STATE_FILE: &str = "task_state.json";
pub const TASK_PLAN_FILE: &str = ".rusty_claw_task_plan.json";
pub const MEMORY_DB_FILE: &str = ".rusty_claw_memory.db";

pub fn rusty_claw_root() -> PathBuf {
    PathBuf::from(RUSTY_CLAW_DIR)
}

pub fn sessions_root() -> PathBuf {
    rusty_claw_root().join(SESSIONS_DIR)
}

pub fn artifacts_root() -> PathBuf {
    rusty_claw_root().join(ARTIFACTS_DIR)
}

pub fn session_registry_path() -> PathBuf {
    rusty_claw_root().join(SESSIONS_REGISTRY_FILE)
}

pub fn event_log_path(session_id: &str) -> PathBuf {
    sessions_root()
        .join(sanitize_id_component(session_id))
        .join(EVENT_LOG_FILE)
}

pub fn task_state_path(session_id: &str) -> PathBuf {
    sessions_root()
        .join(sanitize_id_component(session_id))
        .join(TASK_STATE_FILE)
}

pub fn task_plan_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(TASK_PLAN_FILE)
}

pub fn memory_db_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(MEMORY_DB_FILE)
}

pub fn sanitize_id_component(raw: &str) -> String {
    raw.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub fn transcript_path_for_session(base_dir: &Path, session_id: &str) -> PathBuf {
    base_dir.join(format!("{}.jsonl", sanitize_id_component(session_id)))
}

pub fn new_task_id() -> String {
    prefixed_uuid("task")
}

pub fn new_turn_id() -> String {
    prefixed_uuid("turn")
}

pub fn new_event_id() -> String {
    prefixed_uuid("evt")
}

pub fn new_run_id() -> String {
    prefixed_uuid("run")
}

pub fn new_artifact_id() -> String {
    prefixed_uuid("art")
}

pub fn new_evidence_id() -> String {
    prefixed_uuid("ev")
}

fn prefixed_uuid(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_id_component() {
        assert_eq!(
            sanitize_id_component("telegram:123/abc"),
            "telegram_123_abc"
        );
    }

    #[test]
    fn test_transcript_path_for_session() {
        let path = transcript_path_for_session(Path::new("rusty_claw/sessions"), "discord:42");
        assert_eq!(path, PathBuf::from("rusty_claw/sessions/discord_42.jsonl"));
    }

    #[test]
    fn test_event_log_path() {
        assert_eq!(
            event_log_path("telegram:123"),
            PathBuf::from("rusty_claw/sessions/telegram_123/events.jsonl")
        );
    }

    #[test]
    fn test_task_state_path() {
        assert_eq!(
            task_state_path("telegram:123"),
            PathBuf::from("rusty_claw/sessions/telegram_123/task_state.json")
        );
    }

    #[test]
    fn test_prefixed_ids() {
        assert!(new_task_id().starts_with("task_"));
        assert!(new_turn_id().starts_with("turn_"));
        assert!(new_event_id().starts_with("evt_"));
        assert!(new_run_id().starts_with("run_"));
        assert!(new_artifact_id().starts_with("art_"));
        assert!(new_evidence_id().starts_with("ev_"));
    }
}
