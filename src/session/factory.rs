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
    session_tools.push(Arc::new(crate::tools::AskUserQuestionTool::new()));
    let subagent_base_tools = session_tools.clone();
    session_tools.push(Arc::new(crate::tools::DispatchSubagentTool::new(
        llm.clone(),
        subagent_base_tools,
    )));

    let mut agent_loop = AgentLoop::new(
        session_id.to_string(),
        llm,
        reply_to.to_string(),
        session_tools,
        context,
        output,
        telemetry,
        task_state_store,
    );
    agent_loop.add_extension(Box::new(crate::skills::runtime::SkillRuntime::new()));

    Ok(Arc::new(AsyncMutex::new(agent_loop)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use tokio::sync::mpsc;

    struct CaptureOutput;

    #[async_trait]
    impl crate::core::AgentOutput for CaptureOutput {
        async fn on_text(&self, _text: &str) {}
        async fn on_tool_start(&self, _name: &str, _args: &str) {}
        async fn on_tool_end(&self, _result: &str) {}
        async fn on_error(&self, _error: &str) {}
    }

    struct InspectingLlm {
        pub last_system: std::sync::Mutex<Option<String>>,
    }

    #[async_trait]
    impl crate::llm_client::LlmClient for InspectingLlm {
        fn model_name(&self) -> &str {
            "inspecting"
        }

        fn provider_name(&self) -> &str {
            "test-provider"
        }

        async fn stream(
            &self,
            _messages: Vec<crate::context::Message>,
            system_instruction: Option<crate::context::Message>,
            _tools: Vec<Arc<dyn Tool>>,
        ) -> Result<mpsc::Receiver<crate::llm_client::StreamEvent>, crate::llm_client::LlmError>
        {
            let text = system_instruction
                .and_then(|message| message.parts.into_iter().find_map(|part| part.text))
                .unwrap_or_default();
            *self.last_system.lock().unwrap() = Some(text);

            let (tx, rx) = mpsc::channel(4);
            tokio::spawn(async move {
                let _ = tx
                    .send(crate::llm_client::StreamEvent::Text(
                        "skill ready".to_string(),
                    ))
                    .await;
                let _ = tx.send(crate::llm_client::StreamEvent::Done).await;
            });
            Ok(rx)
        }
    }

    fn cleanup_session(session_id: &str) {
        let session_dir = crate::schema::StoragePaths::session_dir(session_id);
        let _ = std::fs::remove_dir_all(session_dir);
    }

    #[tokio::test]
    async fn test_built_session_registers_skill_runtime_and_injects_skill_prompt() {
        let session_id = "test-skill-session-factory";
        cleanup_session(session_id);
        let temp_root = std::env::temp_dir().join(format!(
            "rusty_claw_test_{}_{}",
            session_id,
            std::process::id()
        ));
        let transcript_path =
            crate::session::repository::SessionRegistryStore::new(temp_root.clone())
                .transcript_path(session_id);

        let llm = Arc::new(InspectingLlm {
            last_system: std::sync::Mutex::new(None),
        });
        let output = Arc::new(CaptureOutput);
        let agent = build_agent_session(
            session_id,
            "cli",
            llm.clone(),
            Vec::new(),
            transcript_path,
            output,
        )
        .unwrap();

        let mut agent = agent.lock().await;
        let exit = agent.step("/check_git_status".to_string()).await.unwrap();
        assert_eq!(exit, crate::core::RunExit::YieldedToUser);

        let system = llm.last_system.lock().unwrap().clone().unwrap_or_default();
        assert!(system.contains("[ACTIVE SKILL CONTRACT]"));
        assert!(system.contains("[ACTIVE SKILL INSTRUCTIONS]"));
        assert!(system.contains("[ACTIVE SKILL STATE]"));
        assert!(system.contains("check_git_status"));
        cleanup_session(session_id);
        let _ = std::fs::remove_dir_all(temp_root);
    }
}
