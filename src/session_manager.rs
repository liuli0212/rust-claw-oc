use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::core::{AgentLoop, AgentOutput};
use crate::context::AgentContext;
use std::path::PathBuf;
use crate::llm_client::GeminiClient;
use crate::tools::Tool;
use crate::rag::VectorStore;

pub struct SessionManager {
    llm: Arc<GeminiClient>,
    tools: Vec<Arc<dyn Tool>>,
    rag_store: Option<Arc<VectorStore>>,
    sessions: Mutex<HashMap<String, Arc<Mutex<AgentLoop>>>>,
}

impl SessionManager {
    pub fn new(llm: Arc<GeminiClient>, tools: Vec<Arc<dyn Tool>>, rag_store: Option<Arc<VectorStore>>) -> Self {
        Self {
            llm,
            tools,
            rag_store,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub async fn get_or_create_session(&self, session_id: &str, output: Arc<dyn AgentOutput>) -> Arc<Mutex<AgentLoop>> {
        let mut sessions = self.sessions.lock().await;
        if let Some(agent) = sessions.get(session_id) {
            return agent.clone();
        }

        let memory_file_path = PathBuf::from("MEMORY.md");
        let context = AgentContext::new(memory_file_path);
        
        let agent = AgentLoop::new(self.llm.clone(), self.tools.clone(), context, output, self.rag_store.clone());
        let agent = Arc::new(Mutex::new(agent));
        sessions.insert(session_id.to_string(), agent.clone());
        agent
    }
}
