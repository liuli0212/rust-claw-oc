use crate::event_log::{AgentEvent, EventRecord};
use crate::schema::TASK_STATE_SCHEMA_VERSION;
use crate::tools::{TaskPlanItem, TaskPlanState};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::fs;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PlanStep {
    pub step: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskStateSnapshot {
    pub schema_version: u32,
    pub task_id: String,
    pub derived_from_event_id: String,
    pub derived_at: u64,
    pub status: String,
    pub goal: String,
    pub current_step: Option<String>,
    pub plan_steps: Vec<PlanStep>,
    pub artifact_ids: Vec<String>,
}

impl TaskStateSnapshot {
    pub fn empty(task_id: String) -> Self {
        Self {
            schema_version: TASK_STATE_SCHEMA_VERSION,
            task_id,
            derived_from_event_id: String::new(),
            derived_at: 0,
            status: "pending".to_string(),
            goal: String::new(),
            current_step: None,
            plan_steps: Vec::new(),
            artifact_ids: Vec::new(),
        }
    }
}

pub fn replay_task_state(task_id: &str, events: &[EventRecord]) -> TaskStateSnapshot {
    let mut snapshot = TaskStateSnapshot::empty(task_id.to_string());

    for event in events
        .iter()
        .filter(|event| event_task_id(event).as_deref() == Some(task_id))
    {
        snapshot.derived_from_event_id = event.event_id.clone();
        snapshot.derived_at = event.timestamp_unix;

        match event.agent_event() {
            Some(AgentEvent::TaskStarted(payload)) => {
                snapshot.status = "in_progress".to_string();
                snapshot.goal = payload.goal;
            }
            Some(AgentEvent::TaskYielded(_)) => {
                snapshot.status = "waiting_user".to_string();
            }
            Some(AgentEvent::TaskFinished(_)) => {
                snapshot.status = "completed".to_string();
            }
            Some(AgentEvent::TaskFailed(_)) => {
                snapshot.status = "failed".to_string();
            }
            Some(AgentEvent::TaskStopped(_)) => {
                snapshot.status = "stopped".to_string();
            }
            Some(AgentEvent::ToolExecutionStarted(payload)) => {
                snapshot.current_step = Some(format!("Running tool: {}", payload.tool_name));
            }
            Some(AgentEvent::ToolExecutionFinished(_)) => {
                snapshot.current_step = None;
            }
            Some(AgentEvent::TaskPlanSynced(payload)) => {
                if let Some(plan_state) = parse_plan_state_from_value(payload.plan_state) {
                    snapshot.plan_steps = plan_state
                        .items
                        .iter()
                        .map(|item| PlanStep {
                            step: item.step.clone(),
                            status: item.status.clone(),
                            note: item.note.clone(),
                        })
                        .collect();
                    snapshot.current_step = current_plan_step(&plan_state.items);
                }
            }
            Some(AgentEvent::ArtifactCreated(payload)) => {
                if !snapshot.artifact_ids.contains(&payload.artifact_id) {
                    snapshot.artifact_ids.push(payload.artifact_id);
                }
            }
            _ => {}
        }
    }

    snapshot
}

pub async fn write_task_state(path: &Path, snapshot: &TaskStateSnapshot) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let json = serde_json::to_string_pretty(snapshot)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    fs::write(path, json).await
}

pub fn read_task_state(path: &Path) -> std::io::Result<TaskStateSnapshot> {
    let content = std::fs::read_to_string(path)?;
    serde_json::from_str(&content)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
}

pub fn summarize_task_state(snapshot: &TaskStateSnapshot) -> String {
    let mut summary = String::new();
    summary
        .push_str("Use this task state as the primary execution state for the current task.\n\n");
    summary.push_str(&format!("Status: {}\n", snapshot.status));
    if !snapshot.goal.is_empty() {
        summary.push_str(&format!("Goal: {}\n", snapshot.goal));
    }
    if let Some(current_step) = &snapshot.current_step {
        summary.push_str(&format!("Current Step: {}\n", current_step));
    }
    if !snapshot.plan_steps.is_empty() {
        summary.push_str("\nPlan Steps:\n");
        for (index, step) in snapshot.plan_steps.iter().enumerate() {
            summary.push_str(&format!("{}. [{}] {}\n", index + 1, step.status, step.step));
            if let Some(note) = &step.note {
                summary.push_str(&format!("   Note: {}\n", note));
            }
        }
    }
    summary
}

fn event_task_id(event: &EventRecord) -> Option<String> {
    event.task_id()
}

fn parse_plan_state_from_value(value: serde_json::Value) -> Option<TaskPlanState> {
    serde_json::from_value(value).ok()
}

fn current_plan_step(items: &[TaskPlanItem]) -> Option<String> {
    items
        .iter()
        .find(|item| item.status == "in_progress")
        .map(|item| item.step.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::EventRecord;

    #[test]
    fn test_replay_task_state_tracks_status_transitions() {
        let task_id = "task_1";
        let events = vec![
            EventRecord::new(
                "evt_1".to_string(),
                "cli".to_string(),
                "TaskStarted".to_string(),
                1,
                serde_json::json!({"task_id": task_id, "goal": "Fix build"}),
            ),
            EventRecord::new(
                "evt_2".to_string(),
                "cli".to_string(),
                "ToolExecutionStarted".to_string(),
                2,
                serde_json::json!({"task_id": task_id, "tool_name": "read_file"}),
            ),
            EventRecord::new(
                "evt_3".to_string(),
                "cli".to_string(),
                "TaskPlanSynced".to_string(),
                3,
                serde_json::json!({
                    "task_id": task_id,
                    "plan_state": {
                        "items": [
                            {"step": "Read file", "status": "in_progress", "note": null},
                            {"step": "Patch code", "status": "pending", "note": null}
                        ]
                    }
                }),
            ),
        ];

        let snapshot = replay_task_state(task_id, &events);
        assert_eq!(snapshot.schema_version, TASK_STATE_SCHEMA_VERSION);
        assert_eq!(snapshot.status, "in_progress");
        assert_eq!(snapshot.goal, "Fix build");
        assert_eq!(snapshot.derived_from_event_id, "evt_3");
        assert_eq!(snapshot.current_step.as_deref(), Some("Read file"));
        assert_eq!(snapshot.plan_steps.len(), 2);
    }

    #[tokio::test]
    async fn test_write_task_state_persists_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task_state.json");
        let snapshot = TaskStateSnapshot {
            schema_version: TASK_STATE_SCHEMA_VERSION,
            task_id: "task_1".to_string(),
            derived_from_event_id: "evt_9".to_string(),
            derived_at: 9,
            status: "completed".to_string(),
            goal: "Ship it".to_string(),
            current_step: None,
            plan_steps: Vec::new(),
            artifact_ids: Vec::new(),
        };

        write_task_state(&path, &snapshot).await.unwrap();

        let content = tokio::fs::read_to_string(path).await.unwrap();
        assert!(content.contains("\"task_id\": \"task_1\""));
        assert!(content.contains("\"status\": \"completed\""));
    }

    #[test]
    fn test_summarize_task_state_includes_goal_and_status() {
        let snapshot = TaskStateSnapshot {
            schema_version: TASK_STATE_SCHEMA_VERSION,
            task_id: "task_1".to_string(),
            derived_from_event_id: "evt_9".to_string(),
            derived_at: 9,
            status: "in_progress".to_string(),
            goal: "Fix context".to_string(),
            current_step: Some("Running tool: read_file".to_string()),
            plan_steps: Vec::new(),
            artifact_ids: Vec::new(),
        };

        let summary = summarize_task_state(&snapshot);
        assert!(summary.contains("Status: in_progress"));
        assert!(summary.contains("Goal: Fix context"));
    }
}
