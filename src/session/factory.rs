use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex as AsyncMutex;

use crate::context::AgentContext;
use crate::core::{AgentLoop, AgentOutput};
use crate::llm_client::LlmClient;
use crate::tools::Tool;

pub fn build_agent_session(
    session_id: &str,
    reply_to: &str,
    llm: Arc<dyn LlmClient>,
    tools: Vec<Arc<dyn Tool>>,
    transcript_path: PathBuf,
    output: Arc<dyn AgentOutput>,
) -> Result<Arc<AsyncMutex<AgentLoop>>, String> {
    let mut context = AgentContext::new().with_transcript_path(transcript_path);
    let _ = context.load_transcript().map_err(|e| e.to_string())?;

    let (telemetry, _telemetry_handle) = crate::telemetry::TelemetryExporter::new();
    let telemetry = Arc::new(telemetry);
    let task_state_store = Arc::new(crate::task_state::TaskStateStore::new(session_id));

    let mut session_tools = tools;
    session_tools.push(Arc::new(crate::tools::TaskPlanTool::new(
        session_id.to_string(),
        task_state_store.clone(),
    )));
    session_tools.push(Arc::new(crate::tools::FinishTaskTool {
        task_state_store: task_state_store.clone(),
    }));

    Ok(Arc::new(AsyncMutex::new(AgentLoop::new(
        session_id.to_string(),
        llm,
        reply_to.to_string(),
        session_tools,
        context,
        output,
        telemetry,
        task_state_store,
    ))))
}
