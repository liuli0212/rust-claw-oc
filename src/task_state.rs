use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::event_log::AgentEvent;
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

    pub fn apply_event(&mut self, event: &AgentEvent) {
        self.derived_from_event_id = Some(event.event_id.clone());
        self.derived_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        if self.task_id.is_none() && event.task_id.is_some() {
            self.task_id = event.task_id.clone();
        }

        match event.event_type.as_str() {
            "TaskStarted" => {
                self.status = "in_progress".to_string();
                if let Some(goal) = event.payload.get("goal").and_then(|v| v.as_str()) {
                    self.goal = Some(goal.to_string());
                }
            }
            "PlanInitialized" => {
                if let Some(steps) = event.payload.get("steps").and_then(|v| v.as_array()) {
                    self.plan_steps.clear();
                    for s in steps {
                        if let Some(step_text) = s.as_str() {
                            self.plan_steps.push(PlanStep {
                                step: step_text.to_string(),
                                status: "pending".to_string(),
                                note: None,
                            });
                        }
                    }
                }
            }
            "PlanStepAdded" => {
                if let Some(step) = event.payload.get("step").and_then(|v| v.as_str()) {
                    self.plan_steps.push(PlanStep {
                        step: step.to_string(),
                        status: "pending".to_string(),
                        note: None,
                    });
                }
            }
            "PlanStepUpdated" => {
                if let Some(idx) = event.payload.get("index").and_then(|v| v.as_u64()) {
                    let idx = idx as usize;
                    if idx < self.plan_steps.len() {
                        if let Some(status) = event.payload.get("status").and_then(|v| v.as_str()) {
                            self.plan_steps[idx].status = status.to_string();
                        }
                        if let Some(note) = event.payload.get("note").and_then(|v| v.as_str()) {
                            self.plan_steps[idx].note = Some(note.to_string());
                        }
                        if let Some(step_text) = event.payload.get("step").and_then(|v| v.as_str()) {
                            self.plan_steps[idx].step = step_text.to_string();
                        }
                    }
                }
            }
            "PlanStepRemoved" => {
                if let Some(idx) = event.payload.get("index").and_then(|v| v.as_u64()) {
                    let idx = idx as usize;
                    if idx < self.plan_steps.len() {
                        self.plan_steps.remove(idx);
                    }
                }
            }
            "PlanCleared" => {
                self.plan_steps.clear();
            }
            "TaskFinished" => {
                self.status = "completed".to_string();
            }
            "EvidenceAdded" => {
                if let Some(eid) = event.payload.get("evidence_id").and_then(|v| v.as_str()) {
                    if !self.evidence_ids.iter().any(|x| x == eid) {
                        self.evidence_ids.push(eid.to_string());
                    }
                }
            }
            "EvidenceInvalidated" => {
                if let Some(eid) = event.payload.get("evidence_id").and_then(|v| v.as_str()) {
                    self.evidence_ids.retain(|x| x != eid);
                }
            }
            _ => {}
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

    pub fn materialize_from_events(&self, events: &[AgentEvent]) -> TaskStateSnapshot {
        let mut state = TaskStateSnapshot::empty();
        for event in events {
            state.apply_event(event);
        }
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_materialize_state() {
        let e1 = AgentEvent::new(
            "TaskStarted",
            "sess",
            None,
            None,
            serde_json::json!({"goal": "build house"}),
        );
        let e2 = AgentEvent::new(
            "PlanInitialized",
            "sess",
            None,
            None,
            serde_json::json!({"steps": ["buy wood", "build"]}),
        );
        let e3 = AgentEvent::new(
            "PlanStepUpdated",
            "sess",
            None,
            None,
            serde_json::json!({"index": 0, "status": "completed"}),
        );

        let store = TaskStateStore::new("sess");
        let state = store.materialize_from_events(&[e1, e2, e3]);
        assert_eq!(state.status, "in_progress");
        assert_eq!(state.goal.as_deref().unwrap(), "build house");
        assert_eq!(state.plan_steps.len(), 2);
        assert_eq!(state.plan_steps[0].status, "completed");
        assert_eq!(state.plan_steps[1].status, "pending");

        let summary = state.summary();
        assert!(summary.contains("[0] buy wood (completed)"));
        assert!(summary.contains("[1] build (pending)"));
    }
}
