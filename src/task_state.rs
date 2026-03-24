use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::schema::StoragePaths;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanStep {
    pub step: String,
    pub status: String, // pending, in_progress, completed
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct TaskStateSnapshot {
    pub schema_version: u32,
    pub task_id: Option<String>,
    pub derived_from_event_id: Option<String>,
    pub derived_at: u64,
    pub status: String,
    pub goal: Option<String>,
    pub current_step: Option<String>,
    pub finish_summary: Option<String>,
    pub plan_steps: Vec<PlanStep>,
}

impl TaskStateSnapshot {
    pub fn empty() -> Self {
        Self {
            schema_version: crate::schema::CURRENT_SCHEMA_VERSION,
            status: "initialized".to_string(),
            ..Default::default()
        }
    }

    pub fn summary(&self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        
        if self.status != "finished" {
            let _ = writeln!(s, "Current task ({})", self.status);
        }

        if let Some(finish_summary) = &self.finish_summary {
            let _ = writeln!(s, "{}", finish_summary);
        }

        if let Some(goal) = &self.goal {
            let _ = writeln!(s, "- Goal: {}", goal);
        }
        if let Some(step) = &self.current_step {
            let _ = writeln!(s, "- Current step: {}", step);
        } else {
            // Pick first pending/in_progress
            let active = self.plan_steps.iter().find(|p| p.status != "completed");
            if let Some(active) = active {
                let _ = writeln!(s, "- Current step: {} ({})", active.step, active.status);
            }
        }

        if !self.plan_steps.is_empty() && self.status != "finished" {
            let _ = writeln!(s, "\nPlan steps:");
            for (i, p) in self.plan_steps.iter().enumerate() {
                let note = p.note.clone().unwrap_or_default();
                let doc = if note.is_empty() {
                    " ".to_string()
                } else {
                    format!(" - {}", note)
                };
                let _ = writeln!(s, "  [{}] {} ({}){}", i, p.step, p.status, doc);
            }
        }

        s.trim().to_string()
    }
}

pub struct TaskStateStore {
    file_path: PathBuf,
}

impl TaskStateStore {
    pub fn new(session_id: &str) -> Self {
        Self {
            file_path: StoragePaths::task_state_file(session_id),
        }
    }

    pub fn clear(&self) -> Result<(), std::io::Error> {
        if self.file_path.exists() {
            std::fs::remove_file(&self.file_path)?;
        }
        Ok(())
    }

    pub fn load(&self) -> Result<TaskStateSnapshot, std::io::Error> {
        if !self.file_path.exists() {
            return Ok(TaskStateSnapshot::empty());
        }
        let content = fs::read_to_string(&self.file_path)?;
        let state = serde_json::from_str(&content).unwrap_or_else(|_| TaskStateSnapshot::empty());
        Ok(state)
    }

    pub fn save(&self, state: &TaskStateSnapshot) -> Result<(), std::io::Error> {
        if let Some(parent) = self.file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(state)?;
        fs::write(&self.file_path, content)?;
        Ok(())
    }

    pub fn has_active_plan(&self) -> bool {
        if let Ok(state) = self.load() {
            !state.plan_steps.is_empty() && state.status == "in_progress"
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_summary_uses_first_incomplete_plan_step_when_current_step_missing() {
        let state = TaskStateSnapshot {
            status: "in_progress".to_string(),
            goal: Some("Refactor safely".to_string()),
            current_step: None,
            plan_steps: vec![
                PlanStep {
                    step: "Old completed step".to_string(),
                    status: "completed".to_string(),
                    note: None,
                },
                PlanStep {
                    step: "Add regression tests".to_string(),
                    status: "pending".to_string(),
                    note: Some("Protect refactor".to_string()),
                },
            ],
            ..TaskStateSnapshot::empty()
        };

        let summary = state.summary();

        assert!(summary.contains("Current task (in_progress)"));
        assert!(summary.contains("- Goal: Refactor safely"));
        assert!(summary.contains("- Current step: Add regression tests (pending)"));
        assert!(summary.contains("[1] Add regression tests (pending) - Protect refactor"));
    }

    #[test]
    fn test_load_invalid_json_falls_back_to_empty_snapshot() {
        let dir = tempdir().unwrap();
        let store = TaskStateStore {
            file_path: dir.path().join("task_state.json"),
        };

        fs::write(&store.file_path, "{ not valid json").unwrap();

        let loaded = store.load().unwrap();

        assert_eq!(loaded, TaskStateSnapshot::empty());
    }

    #[test]
    fn test_has_active_plan_requires_in_progress_status() {
        let dir = tempdir().unwrap();
        let store = TaskStateStore {
            file_path: dir.path().join("task_state.json"),
        };

        let active_state = TaskStateSnapshot {
            status: "in_progress".to_string(),
            plan_steps: vec![PlanStep {
                step: "Do work".to_string(),
                status: "pending".to_string(),
                note: None,
            }],
            ..TaskStateSnapshot::empty()
        };
        store.save(&active_state).unwrap();
        assert!(store.has_active_plan());

        let inactive_state = TaskStateSnapshot {
            status: "completed".to_string(),
            plan_steps: active_state.plan_steps.clone(),
            ..TaskStateSnapshot::empty()
        };
        store.save(&inactive_state).unwrap();
        assert!(!store.has_active_plan());
    }
}
