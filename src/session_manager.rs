use crate::artifact_store::ArtifactStore;
use crate::context::AgentContext;
use crate::core::{AgentLoop, AgentOutput};
use crate::event_log::{read_event_log_lenient, EventLogWriter};
use crate::llm_client::LlmClient;
use crate::schema::{
    artifacts_root, event_log_path, session_registry_path, sessions_root, task_state_path,
    transcript_path_for_session,
};
use crate::task_state::{replay_task_state, write_task_state};
use crate::telemetry;
use crate::tools::Tool;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex as AsyncMutex;

const ARTIFACT_MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60;
const ARTIFACT_MAX_BYTES: u64 = 500 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
struct SessionRegistry {
    sessions: HashMap<String, SessionEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct SessionEntry {
    transcript_path: String,
    updated_at_unix: u64,
    loaded_turns: usize,
}

pub struct SessionManager {
    llm: Arc<RwLock<Option<Arc<dyn LlmClient>>>>,
    tools: Vec<Arc<dyn Tool>>,
    // Active agent sessions (In-Memory) + their cancel mechanisms
    sessions: AsyncMutex<
        HashMap<
            String,
            (
                Arc<AsyncMutex<AgentLoop>>,
                std::sync::Arc<tokio::sync::Notify>,
                std::sync::Arc<std::sync::atomic::AtomicBool>,
            ),
        >,
    >,
    transcript_dir: PathBuf,
    registry_path: PathBuf,
    // In-Memory Registry Cache (Fast Read/Write)
    registry_cache: Arc<RwLock<SessionRegistry>>,
}

impl SessionManager {
    pub fn new(llm: Option<Arc<dyn LlmClient>>, tools: Vec<Arc<dyn Tool>>) -> Self {
        let transcript_dir = sessions_root();
        let registry_path = session_registry_path();

        // Initial load of registry
        let registry = if registry_path.exists() {
            match fs::read_to_string(&registry_path) {
                Ok(content) => {
                    serde_json::from_str::<SessionRegistry>(&content).unwrap_or_default()
                }
                Err(_) => SessionRegistry::default(),
            }
        } else {
            SessionRegistry::default()
        };

        Self {
            llm: Arc::new(RwLock::new(llm)),
            tools,
            sessions: AsyncMutex::new(HashMap::new()),
            transcript_dir,
            registry_path,
            registry_cache: Arc::new(RwLock::new(registry)),
        }
    }

    pub async fn reset_session(&self, session_id: &str) {
        let mut sessions = self.sessions.lock().await;

        // Remove from memory
        sessions.remove(session_id);

        // Delete transcript file if it exists
        let transcript_path = transcript_path_for_session(&self.transcript_dir, session_id);
        if transcript_path.exists() {
            let _ = std::fs::remove_file(&transcript_path);
        }
        let event_log_path = event_log_path(session_id);
        if event_log_path.exists() {
            let _ = std::fs::remove_file(&event_log_path);
            if let Some(parent) = event_log_path.parent() {
                let _ = std::fs::remove_dir(parent);
            }
        }
        let task_state_path = task_state_path(session_id);
        if task_state_path.exists() {
            let _ = std::fs::remove_file(&task_state_path);
        }
        let artifact_dir = artifacts_root().join(session_id);
        if artifact_dir.exists() {
            let _ = std::fs::remove_dir_all(&artifact_dir);
        }

        // Remove from registry
        if let Ok(mut registry) = self.registry_cache.write() {
            registry.sessions.remove(session_id);
        }
        self.persist_registry_async();
    }

    pub async fn cancel_session(&self, session_id: &str) {
        let sessions = self.sessions.lock().await;
        if let Some((_, notify, cancelled)) = sessions.get(session_id) {
            cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
            notify.notify_waiters();
            tracing::info!("Cancel requested for session: {}", session_id);
        }
    }

    pub async fn get_or_create_session(
        &self,
        session_id: &str,
        output: Arc<dyn AgentOutput>,
    ) -> Result<Arc<AsyncMutex<AgentLoop>>, String> {
        let mut sessions = self.sessions.lock().await;
        if let Some((agent, _, _)) = sessions.get(session_id) {
            // Update timestamp in memory only (fast)
            self.update_registry_entry(session_id, None, None);
            self.persist_registry_async();
            return Ok(agent.clone());
        }

        let transcript_path = transcript_path_for_session(&self.transcript_dir, session_id);
        let task_state_file = task_state_path(session_id);
        self.restore_task_state_if_needed(session_id, &task_state_file)
            .await;
        let mut context = AgentContext::new()
            .with_transcript_path(transcript_path.clone())
            .with_task_state_path(task_state_file.clone());
        let loaded_turns = context.load_transcript().unwrap_or(0);

        // Update registry in memory + trigger async persist
        self.update_registry_entry(session_id, Some(transcript_path), Some(loaded_turns));
        self.persist_registry_async();

        let llm = {
            let llm_guard = self.llm.read().unwrap();
            llm_guard.as_ref().cloned().ok_or_else(|| {
                "No LLM provider configured. Use /model <provider> to set one.".to_string()
            })?
        };

        let event_log = match EventLogWriter::for_session(session_id).await {
            Ok(writer) => Some(Arc::new(writer)),
            Err(err) => {
                tracing::warn!(
                    "Failed to initialize event log for session {}: {}",
                    session_id,
                    err
                );
                None
            }
        };
        let artifact_store_impl = ArtifactStore::new(artifacts_root());
        match artifact_store_impl.cleanup(unix_now(), ARTIFACT_MAX_AGE_SECS, ARTIFACT_MAX_BYTES) {
            Ok(stats) => telemetry::record_artifact_cleanup(&stats),
            Err(err) => {
                tracing::warn!("Failed to cleanup artifact store: {}", err);
            }
        }
        let artifact_store = Some(Arc::new(artifact_store_impl));
        let agent_loop = AgentLoop::new(
            session_id.to_string(),
            llm.clone(),
            self.tools.clone(),
            context,
            output,
            event_log,
            task_state_file,
            artifact_store,
        );
        let token = agent_loop.cancel_token.clone();
        let cancelled = agent_loop.cancelled.clone();
        let agent = Arc::new(AsyncMutex::new(agent_loop));
        sessions.insert(session_id.to_string(), (agent.clone(), token, cancelled));
        Ok(agent)
    }

    async fn restore_task_state_if_needed(&self, session_id: &str, task_state_file: &PathBuf) {
        if task_state_file.exists() {
            return;
        }

        let log_path = event_log_path(session_id);
        if !log_path.exists() {
            return;
        }

        let events = match read_event_log_lenient(&log_path).await {
            Ok(events) => events,
            Err(err) => {
                tracing::warn!("Failed to read event log for session restore: {}", err);
                return;
            }
        };

        let Some(task_id) = events.iter().rev().find_map(extract_task_id) else {
            return;
        };

        let snapshot = replay_task_state(&task_id, &events);
        if let Err(err) = write_task_state(task_state_file, &snapshot).await {
            tracing::warn!(
                "Failed to restore task state for session {}: {}",
                session_id,
                err
            );
        }
    }

    fn update_registry_entry(
        &self,
        session_id: &str,
        transcript_path: Option<PathBuf>,
        loaded_turns: Option<usize>,
    ) {
        let mut cache = self.registry_cache.write().unwrap();
        let entry = cache
            .sessions
            .entry(session_id.to_string())
            .or_insert(SessionEntry {
                transcript_path: transcript_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                updated_at_unix: unix_now(),
                loaded_turns: loaded_turns.unwrap_or(0),
            });

        if let Some(path) = transcript_path {
            entry.transcript_path = path.display().to_string();
        }
        if let Some(turns) = loaded_turns {
            entry.loaded_turns = turns;
        }
        entry.updated_at_unix = unix_now();
    }

    fn persist_registry_async(&self) {
        // Clone data for the background thread
        let registry_path = self.registry_path.clone();
        let cache_snapshot = self.registry_cache.read().unwrap().clone();

        tokio::spawn(async move {
            if let Some(parent) = registry_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let serialized = serde_json::to_string_pretty(&cache_snapshot)
                .unwrap_or_else(|_| "{\"sessions\":{}}".to_string());
            // Write atomically? For now, direct write is fine for MVP.
            let _ = fs::write(&registry_path, serialized);
        });
    }
    pub async fn update_session_llm(
        &self,
        session_id: &str,
        provider: &str,
        model: Option<String>,
    ) -> Result<String, String> {
        let config = crate::config::AppConfig::load();
        // We use the factory function from llm_client
        match crate::llm_client::create_llm_client(provider, model.clone(), &config) {
            Ok(new_llm) => {
                // Update global default for new sessions
                {
                    let mut llm_guard = self.llm.write().unwrap();
                    *llm_guard = Some(new_llm.clone());
                }

                let sessions = self.sessions.lock().await;
                if let Some((agent_mutex, _, _)) = sessions.get(session_id) {
                    let mut agent = agent_mutex.lock().await;
                    agent.update_llm(new_llm);
                    Ok(format!(
                        "Updated session '{}' and global default to provider '{}' model '{:?}'",
                        session_id, provider, model
                    ))
                } else {
                    // Even if session doesn't exist, we updated the global default.
                    Ok(format!("Updated global default to provider '{}' model '{:?}'. (Session '{}' not yet active)", provider, model, session_id))
                }
            }
            Err(e) => Err(e),
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn extract_task_id(event: &crate::event_log::EventRecord) -> Option<String> {
    event.task_id()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::{AgentEvent, EventRecord, TaskStartedPayload, TaskYieldedPayload};
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;

    fn cwd_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[tokio::test]
    async fn test_restore_task_state_from_event_log() {
        let _guard = cwd_lock().lock().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        let dir = tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let manager = SessionManager::new(None, Vec::new());
        let session_id = "cli";
        let log_path = event_log_path(session_id);
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let content = vec![
            EventRecord::from_agent_event(
                "evt_1".to_string(),
                session_id.to_string(),
                1,
                AgentEvent::TaskStarted(TaskStartedPayload {
                    task_id: "task_1".to_string(),
                    turn_id: "turn_1".to_string(),
                    goal: "Restore state".to_string(),
                }),
            ),
            EventRecord::from_agent_event(
                "evt_2".to_string(),
                session_id.to_string(),
                2,
                AgentEvent::TaskYielded(TaskYieldedPayload {
                    task_id: "task_1".to_string(),
                }),
            ),
        ]
        .into_iter()
        .map(|event| serde_json::to_string(&event).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
        fs::write(&log_path, format!("{}\n", content)).unwrap();

        let state_path = task_state_path(session_id);
        manager
            .restore_task_state_if_needed(session_id, &state_path)
            .await;

        let restored = std::fs::read_to_string(&state_path).unwrap();
        assert!(restored.contains("\"goal\": \"Restore state\""));
        assert!(restored.contains("\"status\": \"waiting_user\""));

        std::env::set_current_dir(old_cwd).unwrap();
    }
}
