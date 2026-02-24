use crate::context::{transcript_path_for_session, AgentContext};
use crate::core::{AgentLoop, AgentOutput};
use crate::llm_client::GeminiClient;
use crate::tools::Tool;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Serialize, Deserialize, Default)]
struct SessionRegistry {
    sessions: HashMap<String, SessionEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SessionEntry {
    transcript_path: String,
    updated_at_unix: u64,
    loaded_turns: usize,
}

pub struct SessionManager {
    llm: Arc<GeminiClient>,
    tools: Vec<Arc<dyn Tool>>,
    sessions: Mutex<HashMap<String, Arc<Mutex<AgentLoop>>>>,
    transcript_dir: PathBuf,
    registry_path: PathBuf,
}

impl SessionManager {
    pub fn new(llm: Arc<GeminiClient>, tools: Vec<Arc<dyn Tool>>) -> Self {
        let transcript_dir = PathBuf::from(".rusty_claw").join("sessions");
        let registry_path = PathBuf::from(".rusty_claw").join("sessions.json");
        Self {
            llm,
            tools,
            sessions: Mutex::new(HashMap::new()),
            transcript_dir,
            registry_path,
        }
    }

    pub async fn get_or_create_session(
        &self,
        session_id: &str,
        output: Arc<dyn AgentOutput>,
    ) -> Arc<Mutex<AgentLoop>> {
        let mut sessions = self.sessions.lock().await;
        if let Some(agent) = sessions.get(session_id) {
            let _ = self.upsert_registry(session_id, None, None);
            return agent.clone();
        }

        let transcript_path = transcript_path_for_session(&self.transcript_dir, session_id);
        let mut context = AgentContext::new().with_transcript_path(transcript_path.clone());
        let loaded_turns = context.load_transcript().unwrap_or(0);
        let _ = self.upsert_registry(session_id, Some(transcript_path), Some(loaded_turns));

        let agent = AgentLoop::new(self.llm.clone(), self.tools.clone(), context, output);
        let agent = Arc::new(Mutex::new(agent));
        sessions.insert(session_id.to_string(), agent.clone());
        agent
    }

    fn upsert_registry(
        &self,
        session_id: &str,
        transcript_path: Option<PathBuf>,
        loaded_turns: Option<usize>,
    ) -> std::io::Result<()> {
        if let Some(parent) = self.registry_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut registry = if self.registry_path.exists() {
            match fs::read_to_string(&self.registry_path) {
                Ok(content) => {
                    serde_json::from_str::<SessionRegistry>(&content).unwrap_or_default()
                }
                Err(_) => SessionRegistry::default(),
            }
        } else {
            SessionRegistry::default()
        };

        let entry = registry
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

        let serialized = serde_json::to_string_pretty(&registry)
            .unwrap_or_else(|_| "{\"sessions\":{}}".to_string());
        fs::write(&self.registry_path, serialized)?;
        Ok(())
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
