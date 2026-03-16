use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::schema::{StoragePaths, CURRENT_SCHEMA_VERSION};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    pub schema_version: u32,
    pub event_id: String,
    pub event_type: String,
    pub session_id: String,
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    pub timestamp: u64,
    pub payload: Value,
}

impl AgentEvent {
    pub fn new(
        event_type: impl Into<String>,
        session_id: impl Into<String>,
        task_id: Option<String>,
        turn_id: Option<String>,
        payload: Value,
    ) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            event_id: format!("evt_{}", Uuid::new_v4().simple()),
            event_type: event_type.into(),
            session_id: session_id.into(),
            task_id,
            turn_id,
            timestamp,
            payload,
        }
    }
}

pub struct EventLog {
    file_path: PathBuf,
    // Async mutex ensures serialized appends, avoiding interleaved JSON lines
    writer_mutex: Arc<Mutex<Option<File>>>,
}

impl EventLog {
    pub fn new(session_id: &str) -> Self {
        Self {
            file_path: StoragePaths::events_file(session_id),
            writer_mutex: Arc::new(Mutex::new(None)),
        }
    }

    async fn ensure_writer(
        file_path: &PathBuf,
        guard: &mut tokio::sync::MutexGuard<'_, Option<File>>,
    ) -> std::io::Result<()> {
        if guard.is_none() {
            if let Some(parent) = file_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(file_path)
                .await?;
            **guard = Some(file);
        }
        Ok(())
    }

    pub async fn append(&self, event: AgentEvent) -> std::io::Result<()> {
        // We use serde_json::to_string which avoids introducing pretty printing newlines
        let json_line = serde_json::to_string(&event)?;

        // Serialized write using async Mutex over the writer guard
        let mut guard = self.writer_mutex.lock().await;

        Self::ensure_writer(&self.file_path, &mut guard).await?;

        if let Some(ref mut file) = *guard {
            file.write_all(json_line.as_bytes()).await?;
            file.write_all(b"\n").await?;
            file.flush().await?; // Flush eagerly to make it visible to readers
        }
        Ok(())
    }

    pub async fn read_all(&self) -> std::io::Result<Vec<AgentEvent>> {
        if !self.file_path.exists() {
            return Ok(Vec::new());
        }

        let file = File::open(&self.file_path).await?;
        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        let mut events = Vec::new();

        while let Some(line) = lines.next_line().await? {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<AgentEvent>(line) {
                events.push(event);
            }
        }

        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_event_log_append_and_read() {
        let dir = tempdir().unwrap();
        // Override the path slightly for isolation, we can just set current dir or use an absolute stub.
        // For the sake of test we'll mock `StoragePaths` output if needed, or simply let it write to `rusty_claw` locally,
        // but it's better to isolate. Let's create an EventLog instance with an explicit path:

        let path = dir.path().join("events.jsonl");
        let log = EventLog {
            file_path: path.clone(),
            writer_mutex: Arc::new(Mutex::new(None)),
        };

        log.append(AgentEvent::new(
            "TestEvent",
            "sess1",
            None,
            None,
            serde_json::json!({"msg": "hello"}),
        ))
        .await
        .unwrap();

        let events = log.read_all().await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "TestEvent");
        assert_eq!(events[0].session_id, "sess1");
    }

    #[tokio::test]
    async fn test_event_log_concurrent_append() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = Arc::new(EventLog {
            file_path: path.clone(),
            writer_mutex: Arc::new(Mutex::new(None)),
        });

        let mut handles = vec![];
        for i in 0..100 {
            let log_clone = log.clone();
            handles.push(tokio::spawn(async move {
                log_clone
                    .append(AgentEvent::new(
                        "ConcurrentEvent",
                        "sess1",
                        Some(format!("task{}", i)),
                        None,
                        serde_json::json!({"idx": i}),
                    ))
                    .await
                    .unwrap();
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let events = log.read_all().await.unwrap();
        assert_eq!(events.len(), 100);
    }

    #[tokio::test]
    async fn test_event_log_read_all_skips_invalid_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let valid = serde_json::to_string(&AgentEvent::new(
            "ValidEvent",
            "sess1",
            None,
            None,
            serde_json::json!({"ok": true}),
        ))
        .unwrap();

        tokio::fs::write(
            &path,
            format!("{valid}\nnot-json\n\n{{\"missing\":\"fields\"}}\n"),
        )
        .await
        .unwrap();

        let log = EventLog {
            file_path: path,
            writer_mutex: Arc::new(Mutex::new(None)),
        };

        let events = log.read_all().await.unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "ValidEvent");
    }
}
