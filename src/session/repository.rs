use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::context::transcript_path_for_session;

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

pub struct SessionRegistryStore {
    transcript_dir: PathBuf,
    registry_path: PathBuf,
    cache: Arc<RwLock<SessionRegistry>>,
}

impl SessionRegistryStore {
    pub fn new(root_dir: PathBuf) -> Self {
        let transcript_dir = root_dir.join("sessions");
        let registry_path = root_dir.join("sessions.json");
        let registry = if registry_path.exists() {
            fs::read_to_string(&registry_path)
                .ok()
                .and_then(|content| serde_json::from_str::<SessionRegistry>(&content).ok())
                .unwrap_or_default()
        } else {
            SessionRegistry::default()
        };

        Self {
            transcript_dir,
            registry_path,
            cache: Arc::new(RwLock::new(registry)),
        }
    }

    pub fn transcript_path(&self, session_id: &str) -> PathBuf {
        transcript_path_for_session(&self.transcript_dir, session_id)
    }

    pub fn remove_session_artifacts(&self, session_id: &str) {
        let transcript_path = self.transcript_path(session_id);
        if transcript_path.exists() {
            let _ = std::fs::remove_file(&transcript_path);
        }
        if let Ok(mut cache) = self.cache.write() {
            cache.sessions.remove(session_id);
        }
        self.persist_async();
    }

    pub fn touch_session(
        &self,
        session_id: &str,
        transcript_path: Option<&Path>,
        loaded_turns: Option<usize>,
    ) {
        let mut cache = self.cache.write().unwrap();
        let entry = cache
            .sessions
            .entry(session_id.to_string())
            .or_insert(SessionEntry {
                transcript_path: transcript_path
                    .map(|path| path.display().to_string())
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
        drop(cache);
        self.persist_async();
    }

    pub fn list_sessions(&self) -> Vec<(String, u64, usize)> {
        let cache = self.cache.read().unwrap();
        let mut list: Vec<_> = cache
            .sessions
            .iter()
            .map(|(id, entry)| (id.clone(), entry.updated_at_unix, entry.loaded_turns))
            .collect();
        list.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        list
    }

    fn persist_async(&self) {
        let registry_path = self.registry_path.clone();
        let snapshot = self.cache.read().unwrap().clone();

        tokio::spawn(async move {
            if let Some(parent) = registry_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let serialized = serde_json::to_string_pretty(&snapshot)
                .unwrap_or_else(|_| "{\"sessions\":{}}".to_string());
            let _ = fs::write(&registry_path, serialized);
        });
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}
