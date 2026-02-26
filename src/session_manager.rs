use crate::context::{transcript_path_for_session, AgentContext};
use crate::core::{AgentLoop, AgentOutput};
use crate::llm_client::LlmClient;
use crate::tools::Tool;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex as AsyncMutex;

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
    llm: Arc<dyn LlmClient>,
    tools: Vec<Arc<dyn Tool>>,
    // Active agent sessions (In-Memory) + their cancel tokens
    sessions: AsyncMutex<HashMap<String, (Arc<AsyncMutex<AgentLoop>>, std::sync::Arc<std::sync::atomic::AtomicBool>)>>,
    transcript_dir: PathBuf,
    registry_path: PathBuf,
    // In-Memory Registry Cache (Fast Read/Write)
    registry_cache: Arc<RwLock<SessionRegistry>>,
}

impl SessionManager {
    pub fn new(llm: Arc<dyn LlmClient>, tools: Vec<Arc<dyn Tool>>) -> Self {
        let transcript_dir = PathBuf::from("rusty_claw").join("sessions");
        let registry_path = PathBuf::from("rusty_claw").join("sessions.json");

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
            llm,
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
        
        // Remove from registry
        if let Ok(mut registry) = self.registry_cache.write() {
            registry.sessions.remove(session_id);
        }
        self.persist_registry_async();
    }

    pub async fn cancel_session(&self, session_id: &str) {
        let sessions = self.sessions.lock().await;
        if let Some((_, token)) = sessions.get(session_id) {
            token.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    pub async fn get_or_create_session(
        &self,
        session_id: &str,
        output: Arc<dyn AgentOutput>,
    ) -> Arc<AsyncMutex<AgentLoop>> {
        let mut sessions = self.sessions.lock().await;
        if let Some((agent, _)) = sessions.get(session_id) {
            // Update timestamp in memory only (fast)
            self.update_registry_entry(session_id, None, None);
            self.persist_registry_async();
            return agent.clone();
        }

        let transcript_path = transcript_path_for_session(&self.transcript_dir, session_id);
        let mut context = AgentContext::new().with_transcript_path(transcript_path.clone());
        let loaded_turns = context.load_transcript().unwrap_or(0);

        // Update registry in memory + trigger async persist
        self.update_registry_entry(session_id, Some(transcript_path), Some(loaded_turns));
        self.persist_registry_async();

        let agent_loop = AgentLoop::new(self.llm.clone(), self.tools.clone(), context, output);
        let token = agent_loop.cancel_token.clone();
        let agent = Arc::new(AsyncMutex::new(agent_loop));
        sessions.insert(session_id.to_string(), (agent.clone(), token));
        agent
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
    pub async fn update_session_llm(&self, session_id: &str, provider: &str, model: Option<String>) -> Result<String, String> {
        let config = crate::config::AppConfig::load();
        // We use the factory function from llm_client
        match crate::llm_client::create_llm_client(provider, model.clone(), &config) {
            Ok(new_llm) => {
                let sessions = self.sessions.lock().await;
                if let Some((agent_mutex, _)) = sessions.get(session_id) {
                    let mut agent = agent_mutex.lock().await;
                    agent.update_llm(new_llm);
                    Ok(format!("Updated session '{}' to provider '{}' model '{:?}'", session_id, provider, model))
                } else {
                    Err(format!("Session '{}' not found", session_id))
                }
            }
            Err(e) => Err(e)
        }
    }
}


fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
