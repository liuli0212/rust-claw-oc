use crate::schema::{event_log_path, EVENT_LOG_SCHEMA_VERSION};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub schema_version: u32,
    pub event_id: String,
    pub session_id: String,
    pub event_type: String,
    pub timestamp_unix: u64,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskStartedPayload {
    pub task_id: String,
    pub turn_id: String,
    pub goal: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskStoppedPayload {
    pub task_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskFailedPayload {
    pub task_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskYieldedPayload {
    pub task_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskFinishedPayload {
    pub task_id: String,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolExecutionStartedPayload {
    pub task_id: String,
    pub turn_id: Option<String>,
    pub tool_name: String,
    pub tool_call_id: Option<String>,
    pub args: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolExecutionFinishedPayload {
    pub task_id: String,
    pub tool_name: String,
    pub tool_call_id: Option<String>,
    pub status: String,
    pub result_preview: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskPlanSyncedPayload {
    pub task_id: String,
    pub plan_state: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArtifactCreatedPayload {
    pub task_id: String,
    pub tool_name: String,
    pub artifact_id: String,
    pub artifact_path: String,
    pub is_truncated: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TimelineEntry {
    pub event_id: String,
    pub timestamp_unix: u64,
    pub event_type: String,
    pub task_id: Option<String>,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    TaskStarted(TaskStartedPayload),
    TaskStopped(TaskStoppedPayload),
    TaskFailed(TaskFailedPayload),
    TaskYielded(TaskYieldedPayload),
    TaskFinished(TaskFinishedPayload),
    ToolExecutionStarted(ToolExecutionStartedPayload),
    ToolExecutionFinished(ToolExecutionFinishedPayload),
    TaskPlanSynced(TaskPlanSyncedPayload),
    ArtifactCreated(ArtifactCreatedPayload),
}

impl EventRecord {
    #[cfg(test)]
    pub fn new(
        event_id: String,
        session_id: String,
        event_type: String,
        timestamp_unix: u64,
        payload: Value,
    ) -> Self {
        Self {
            schema_version: EVENT_LOG_SCHEMA_VERSION,
            event_id,
            session_id,
            event_type,
            timestamp_unix,
            payload,
        }
    }

    pub fn from_agent_event(
        event_id: String,
        session_id: String,
        timestamp_unix: u64,
        event: AgentEvent,
    ) -> Self {
        Self {
            schema_version: EVENT_LOG_SCHEMA_VERSION,
            event_id,
            session_id,
            event_type: event.event_type().to_string(),
            timestamp_unix,
            payload: event.payload(),
        }
    }

    pub fn agent_event(&self) -> Option<AgentEvent> {
        AgentEvent::from_record(self)
    }

    pub fn task_id(&self) -> Option<String> {
        self.agent_event().and_then(|event| event.task_id())
    }
}

impl AgentEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            AgentEvent::TaskStarted(_) => "TaskStarted",
            AgentEvent::TaskStopped(_) => "TaskStopped",
            AgentEvent::TaskFailed(_) => "TaskFailed",
            AgentEvent::TaskYielded(_) => "TaskYielded",
            AgentEvent::TaskFinished(_) => "TaskFinished",
            AgentEvent::ToolExecutionStarted(_) => "ToolExecutionStarted",
            AgentEvent::ToolExecutionFinished(_) => "ToolExecutionFinished",
            AgentEvent::TaskPlanSynced(_) => "TaskPlanSynced",
            AgentEvent::ArtifactCreated(_) => "ArtifactCreated",
        }
    }

    pub fn payload(&self) -> Value {
        match self {
            AgentEvent::TaskStarted(payload) => serde_json::to_value(payload),
            AgentEvent::TaskStopped(payload) => serde_json::to_value(payload),
            AgentEvent::TaskFailed(payload) => serde_json::to_value(payload),
            AgentEvent::TaskYielded(payload) => serde_json::to_value(payload),
            AgentEvent::TaskFinished(payload) => serde_json::to_value(payload),
            AgentEvent::ToolExecutionStarted(payload) => serde_json::to_value(payload),
            AgentEvent::ToolExecutionFinished(payload) => serde_json::to_value(payload),
            AgentEvent::TaskPlanSynced(payload) => serde_json::to_value(payload),
            AgentEvent::ArtifactCreated(payload) => serde_json::to_value(payload),
        }
        .unwrap_or(Value::Null)
    }

    pub fn from_record(record: &EventRecord) -> Option<Self> {
        match record.event_type.as_str() {
            "TaskStarted" => serde_json::from_value(record.payload.clone())
                .ok()
                .map(AgentEvent::TaskStarted),
            "TaskStopped" => serde_json::from_value(record.payload.clone())
                .ok()
                .map(AgentEvent::TaskStopped),
            "TaskFailed" => serde_json::from_value(record.payload.clone())
                .ok()
                .map(AgentEvent::TaskFailed),
            "TaskYielded" => serde_json::from_value(record.payload.clone())
                .ok()
                .map(AgentEvent::TaskYielded),
            "TaskFinished" => serde_json::from_value(record.payload.clone())
                .ok()
                .map(AgentEvent::TaskFinished),
            "ToolExecutionStarted" => serde_json::from_value(record.payload.clone())
                .ok()
                .map(AgentEvent::ToolExecutionStarted),
            "ToolExecutionFinished" => serde_json::from_value(record.payload.clone())
                .ok()
                .map(AgentEvent::ToolExecutionFinished),
            "TaskPlanSynced" => serde_json::from_value(record.payload.clone())
                .ok()
                .map(AgentEvent::TaskPlanSynced),
            "ArtifactCreated" => serde_json::from_value(record.payload.clone())
                .ok()
                .map(AgentEvent::ArtifactCreated),
            _ => None,
        }
    }

    pub fn task_id(&self) -> Option<String> {
        Some(match self {
            AgentEvent::TaskStarted(payload) => payload.task_id.clone(),
            AgentEvent::TaskStopped(payload) => payload.task_id.clone(),
            AgentEvent::TaskFailed(payload) => payload.task_id.clone(),
            AgentEvent::TaskYielded(payload) => payload.task_id.clone(),
            AgentEvent::TaskFinished(payload) => payload.task_id.clone(),
            AgentEvent::ToolExecutionStarted(payload) => payload.task_id.clone(),
            AgentEvent::ToolExecutionFinished(payload) => payload.task_id.clone(),
            AgentEvent::TaskPlanSynced(payload) => payload.task_id.clone(),
            AgentEvent::ArtifactCreated(payload) => payload.task_id.clone(),
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn summary(&self) -> String {
        match self {
            AgentEvent::TaskStarted(payload) => format!("task started: {}", payload.goal),
            AgentEvent::TaskStopped(payload) => format!("task stopped: {}", payload.reason),
            AgentEvent::TaskFailed(payload) => format!("task failed: {}", payload.reason),
            AgentEvent::TaskYielded(_) => "task yielded to user".to_string(),
            AgentEvent::TaskFinished(payload) => format!("task finished: {}", payload.summary),
            AgentEvent::ToolExecutionStarted(payload) => {
                format!("tool started: {}", payload.tool_name)
            }
            AgentEvent::ToolExecutionFinished(payload) => {
                format!("tool {}: {}", payload.tool_name, payload.status)
            }
            AgentEvent::TaskPlanSynced(_) => "task plan synced".to_string(),
            AgentEvent::ArtifactCreated(payload) => {
                format!("artifact created: {}", payload.artifact_id)
            }
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn build_task_timeline(events: &[EventRecord], task_id: &str) -> Vec<TimelineEntry> {
    events
        .iter()
        .filter_map(|record| {
            let event = record.agent_event()?;
            if event.task_id().as_deref() != Some(task_id) {
                return None;
            }
            Some(TimelineEntry {
                event_id: record.event_id.clone(),
                timestamp_unix: record.timestamp_unix,
                event_type: record.event_type.clone(),
                task_id: event.task_id(),
                summary: event.summary(),
            })
        })
        .collect()
}

pub struct EventLogWriter {
    path: PathBuf,
    file: Mutex<File>,
}

impl EventLogWriter {
    pub async fn for_session(session_id: &str) -> std::io::Result<Self> {
        Self::new(event_log_path(session_id)).await
    }

    pub async fn new(path: PathBuf) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn append(&self, event: &EventRecord) -> std::io::Result<()> {
        let mut file = self.file.lock().await;
        let line = serde_json::to_string(event)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        file.write_all(line.as_bytes()).await?;
        file.write_all(b"\n").await?;
        file.flush().await
    }
}

pub async fn read_event_log(path: &Path) -> std::io::Result<Vec<EventRecord>> {
    let content = fs::read_to_string(path).await?;
    read_event_log_content(&content, false)
}

pub async fn read_event_log_lenient(path: &Path) -> std::io::Result<Vec<EventRecord>> {
    let content = fs::read_to_string(path).await?;
    read_event_log_content(&content, true)
}

fn read_event_log_content(
    content: &str,
    allow_truncated_tail: bool,
) -> std::io::Result<Vec<EventRecord>> {
    let mut records = Vec::new();
    let lines: Vec<_> = content.lines().collect();
    let last_non_empty_index = lines
        .iter()
        .rposition(|line| !line.trim().is_empty());
    for (index, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record = match serde_json::from_str::<EventRecord>(line) {
            Ok(record) => record,
            Err(err)
                if allow_truncated_tail
                    && Some(index) == last_non_empty_index
                    && err.is_eof() =>
            {
                tracing::warn!(
                    "Ignoring truncated tail event log line {} during replay",
                    index + 1
                );
                break;
            }
            Err(err) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Invalid event log line {}: {}", index + 1, err),
                ));
            }
        };
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_event_log_writer_appends_jsonl() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session").join("events.jsonl");
        let writer = EventLogWriter::new(path.clone()).await.unwrap();
        let event = EventRecord::new(
            "evt_test".to_string(),
            "cli".to_string(),
            "TaskStarted".to_string(),
            123,
            serde_json::json!({"goal": "test"}),
        );

        writer.append(&event).await.unwrap();

        let content = tokio::fs::read_to_string(path).await.unwrap();
        assert!(content.contains("\"event_id\":\"evt_test\""));
        assert!(content.ends_with('\n'));
    }

    #[tokio::test]
    async fn test_read_event_log_replays_records() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session").join("events.jsonl");
        let writer = EventLogWriter::new(path.clone()).await.unwrap();

        writer
            .append(&EventRecord::new(
                "evt_1".to_string(),
                "cli".to_string(),
                "TaskStarted".to_string(),
                1,
                serde_json::json!({"goal": "a"}),
            ))
            .await
            .unwrap();
        writer
            .append(&EventRecord::new(
                "evt_2".to_string(),
                "cli".to_string(),
                "TaskFinished".to_string(),
                2,
                serde_json::json!({"summary": "done"}),
            ))
            .await
            .unwrap();

        let records = read_event_log(&path).await.unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].event_type, "TaskStarted");
        assert_eq!(records[1].event_type, "TaskFinished");
    }

    #[test]
    fn test_agent_event_roundtrip() {
        let record = EventRecord::from_agent_event(
            "evt_1".to_string(),
            "cli".to_string(),
            1,
            AgentEvent::TaskStarted(TaskStartedPayload {
                task_id: "task_1".to_string(),
                turn_id: "turn_1".to_string(),
                goal: "Fix build".to_string(),
            }),
        );
        let event = record.agent_event().unwrap();
        assert_eq!(
            event,
            AgentEvent::TaskStarted(TaskStartedPayload {
                task_id: "task_1".to_string(),
                turn_id: "turn_1".to_string(),
                goal: "Fix build".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn test_read_event_log_rejects_corrupted_line() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session").join("events.jsonl");
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        fs::write(&path, "{\"bad\":true}\nnot-json\n")
            .await
            .unwrap();
        let err = read_event_log(&path).await.unwrap_err();
        assert!(err.to_string().contains("Invalid event log line"));
    }

    #[tokio::test]
    async fn test_read_event_log_lenient_ignores_truncated_tail_line() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session").join("events.jsonl");
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        let first = EventRecord::new(
            "evt_1".to_string(),
            "cli".to_string(),
            "TaskStarted".to_string(),
            1,
            serde_json::json!({"task_id": "task_1", "goal": "ship"}),
        );
        let first_line = serde_json::to_string(&first).unwrap();
        fs::write(&path, format!("{}\n{{\"schema_version\":1", first_line))
            .await
            .unwrap();

        let records = read_event_log_lenient(&path).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].event_id, "evt_1");
    }

    #[test]
    fn test_build_task_timeline_summarizes_events() {
        let events = vec![
            EventRecord::from_agent_event(
                "evt_1".to_string(),
                "cli".to_string(),
                1,
                AgentEvent::TaskStarted(TaskStartedPayload {
                    task_id: "task_1".to_string(),
                    turn_id: "turn_1".to_string(),
                    goal: "Fix build".to_string(),
                }),
            ),
            EventRecord::from_agent_event(
                "evt_2".to_string(),
                "cli".to_string(),
                2,
                AgentEvent::ToolExecutionFinished(ToolExecutionFinishedPayload {
                    task_id: "task_1".to_string(),
                    tool_name: "read_file".to_string(),
                    tool_call_id: Some("call_1".to_string()),
                    status: "ok".to_string(),
                    result_preview: "preview".to_string(),
                }),
            ),
        ];
        let timeline = build_task_timeline(&events, "task_1");
        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline[0].summary, "task started: Fix build");
        assert_eq!(timeline[1].summary, "tool read_file: ok");
    }
}
