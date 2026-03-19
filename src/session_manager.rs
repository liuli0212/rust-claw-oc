use crate::core::{AgentLoop, AgentOutput, OutputRouter};
use crate::llm_client::LlmClient;
use crate::tools::Tool;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex as AsyncMutex;

type SessionEntryMap = AsyncMutex<
    HashMap<
        String,
        (
            Arc<AsyncMutex<AgentLoop>>,
            std::sync::Arc<tokio::sync::Notify>,
            std::sync::Arc<std::sync::atomic::AtomicBool>,
        ),
    >,
>;

pub struct SessionManager {
    llm: Arc<RwLock<Option<Arc<dyn LlmClient>>>>,
    tools: RwLock<Vec<Arc<dyn Tool>>>,
    routers: RwLock<Vec<Arc<dyn OutputRouter>>>,
    sessions: SessionEntryMap,
    registry: crate::session::repository::SessionRegistryStore,
}

impl SessionManager {
    pub fn new(llm: Option<Arc<dyn LlmClient>>, tools: Vec<Arc<dyn Tool>>) -> Self {
        Self {
            llm: Arc::new(RwLock::new(llm)),
            tools: RwLock::new(tools),
            routers: RwLock::new(Vec::new()),
            sessions: AsyncMutex::new(HashMap::new()),
            registry: crate::session::repository::SessionRegistryStore::new(
                std::path::PathBuf::from("rusty_claw"),
            ),
        }
    }

    pub fn add_output_router(&self, router: Arc<dyn OutputRouter>) {
        let mut routers = self.routers.write().unwrap();
        routers.push(router);
    }

    pub fn route_output(&self, reply_to: &str) -> Option<Arc<dyn AgentOutput>> {
        let routers = self.routers.read().unwrap();
        for router in routers.iter() {
            if let Some(output) = router.try_route(reply_to) {
                return Some(output);
            }
        }
        None
    }

    pub fn add_tool(&self, tool: Arc<dyn Tool>) {
        let mut tools = self.tools.write().unwrap();
        tools.push(tool);
    }

    pub async fn reset_session(&self, session_id: &str) {
        let mut sessions = self.sessions.lock().await;

        // Remove from memory
        sessions.remove(session_id);

        self.registry.remove_session_artifacts(session_id);
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
        let existing = {
            let sessions = self.sessions.lock().await;
            sessions.get(session_id).map(|(a, _, _)| a.clone())
        };

        if let Some(agent_mutex) = existing {
            self.registry.touch_session(session_id, None, None);
            return Ok(agent_mutex.clone());
        }

        let transcript_path = self.registry.transcript_path(session_id);

        let llm = {
            let llm_guard = self.llm.read().unwrap();
            llm_guard
                .as_ref()
                .ok_or_else(|| {
                    "No LLM provider configured. Use /model <provider> to set one.".to_string()
                })?
                .clone()
        };
        let tools = self.tools.read().unwrap().clone();
        let agent = crate::session::factory::build_agent_session(
            session_id,
            llm,
            tools,
            transcript_path.clone(),
            output,
        )?;
        let loaded_turns = agent.lock().await.context.dialogue_history.len();
        self.registry
            .touch_session(session_id, Some(&transcript_path), Some(loaded_turns));
        let token = agent.lock().await.cancel_token.clone();
        let cancelled = agent.lock().await.cancelled.clone();

        let mut sessions = self.sessions.lock().await;
        sessions.insert(session_id.to_string(), (agent.clone(), token, cancelled));
        Ok(agent)
    }

    pub async fn update_session_llm(
        &self,
        session_id: &str,
        provider: &str,
        model: Option<String>,
    ) -> Result<String, String> {
        let config = crate::config::AppConfig::load();
        // We use the factory function from llm_client
        match crate::llm_client::create_llm_client(provider, model.clone(), None, &config) {
            Ok(new_llm) => {
                // Update global default for new sessions
                {
                    let mut llm_guard = self.llm.write().unwrap();
                    *llm_guard = Some(new_llm.clone());
                }

                let agent_mutex = {
                    let sessions = self.sessions.lock().await;
                    sessions.get(session_id).map(|(a, _, _)| a.clone())
                };
                if let Some(agent_mutex) = agent_mutex {
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

    #[cfg(feature = "acp")]
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.read().unwrap().clone()
    }

    pub fn list_sessions(&self) -> Vec<(String, u64, usize)> {
        self.registry.list_sessions()
    }
}
