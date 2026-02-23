use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::core::{AgentLoop, AgentOutput};
use crate::context::AgentContext;
use crate::llm_client::GeminiClient;
use crate::tools::Tool;

pub struct SessionManager {
    llm: Arc<GeminiClient>,
    tools: Vec<Arc<dyn Tool>>,
    sessions: Mutex<HashMap<String, Arc<Mutex<AgentLoop>>>>,
}

impl SessionManager {
    pub fn new(llm: Arc<GeminiClient>, tools: Vec<Arc<dyn Tool>>) -> Self {
        Self {
            llm,
            tools,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub async fn get_or_create_session(&self, session_id: &str, output: Arc<dyn AgentOutput>) -> Arc<Mutex<AgentLoop>> {
        let mut sessions = self.sessions.lock().await;
        if let Some(agent) = sessions.get(session_id) {
            return agent.clone();
        }

        let context = AgentContext::new();
        
        let agent = AgentLoop::new(self.llm.clone(), self.tools.clone(), context, output);
        let agent = Arc::new(Mutex::new(agent));
        sessions.insert(session_id.to_string(), agent.clone());
        agent
    }
}
