use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::schema::StoragePaths;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub step: String,
    pub status: String, // pending, in_progress, completed
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TaskStateSnapshot {
    pub schema_version: u32,
    pub task_id: Option<String>,
    pub derived_from_event_id: Option<String>,
    pub derived_at: u64,
    pub status: String,
    pub goal: Option<String>,
    pub current_step: Option<String>,
    pub plan_steps: Vec<PlanStep>,
    pub evidence_ids: Vec<String>,
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
        let _ = writeln!(s, "Current task ({})", self.status);
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

        if !self.plan_steps.is_empty() {
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
