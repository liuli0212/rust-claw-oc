use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;

use crate::context::AgentContext;
use crate::core::{AgentLoop, AgentOutput};
use crate::llm_client::LlmClient;
use crate::tools::{Tool, ToolContext};

const DEFAULT_SUBAGENT_ALLOWED_TOOLS: &[&str] = &["read_file", "web_fetch"];
const FORBIDDEN_ASYNC_SUBAGENT_TOOLS: &[&str] = &[
    "write_file",
    "patch_file",
    "execute_bash",
    "send_file",
    "write_memory",
    "rag_insert",
    "manage_schedule",
    "send_telegram_message",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentBuildMode {
    AsyncReadonly,
    SyncCompatible,
}

pub struct BuiltSubagentSession {
    pub session_id: String,
    pub agent_loop: AgentLoop,
    pub collector: Arc<CollectorOutput>,
}

pub struct CollectorOutput {
    text: AsyncMutex<String>,
    tool_outputs: AsyncMutex<Vec<String>>,
    artifacts: AsyncMutex<Vec<String>>,
}

impl CollectorOutput {
    pub fn new() -> Self {
        Self {
            text: AsyncMutex::new(String::new()),
            tool_outputs: AsyncMutex::new(Vec::new()),
            artifacts: AsyncMutex::new(Vec::new()),
        }
    }

    pub async fn take_text(&self) -> String {
        let mut text = self.text.lock().await;
        std::mem::take(&mut *text)
    }

    pub async fn take_tool_outputs(&self) -> Vec<String> {
        let mut outputs = self.tool_outputs.lock().await;
        std::mem::take(&mut *outputs)
    }

    pub async fn take_artifacts(&self) -> Vec<String> {
        let mut artifacts = self.artifacts.lock().await;
        std::mem::take(&mut *artifacts)
    }
}

#[async_trait]
impl crate::core::AgentOutput for CollectorOutput {
    async fn on_text(&self, text: &str) {
        self.text.lock().await.push_str(text);
    }

    async fn on_thinking(&self, _text: &str) {}

    async fn on_tool_start(&self, name: &str, args: &str) {
        if name == "write_file" || name == "patch_file" {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(args) {
                if let Some(path) = parsed.get("path").and_then(|p| p.as_str()) {
                    self.artifacts.lock().await.push(path.to_string());
                }
            }
        }
    }

    async fn on_tool_end(&self, result: &str) {
        let truncated = if result.len() > 500 {
            format!("{}...(truncated)", &result[..500])
        } else {
            result.to_string()
        };
        self.tool_outputs.lock().await.push(truncated);
    }

    async fn on_error(&self, error: &str) {
        self.text
            .lock()
            .await
            .push_str(&format!("[ERROR] {}\n", error));
    }
}

pub fn filter_subagent_tools(
    base_tools: &[Arc<dyn Tool>],
    allowed: &[String],
    mode: SubagentBuildMode,
) -> Vec<Arc<dyn Tool>> {
    let effective_allowed: Vec<String> = if allowed.is_empty() {
        DEFAULT_SUBAGENT_ALLOWED_TOOLS
            .iter()
            .map(|name| (*name).to_string())
            .collect()
    } else {
        allowed.to_vec()
    };

    let runtime_tools = ["finish_task", "task_plan"];

    base_tools
        .iter()
        .filter(|tool| {
            let name = tool.name();
            if name == "dispatch_subagent" {
                return false;
            }

            if matches!(mode, SubagentBuildMode::AsyncReadonly)
                && FORBIDDEN_ASYNC_SUBAGENT_TOOLS.contains(&name.as_str())
            {
                return false;
            }

            runtime_tools.contains(&name.as_str()) || effective_allowed.contains(&name)
        })
        .cloned()
        .collect()
}

pub fn build_subagent_session(
    parent_ctx: &ToolContext,
    llm: Arc<dyn LlmClient>,
    base_tools: &[Arc<dyn Tool>],
    mode: SubagentBuildMode,
    allowed_tools: &[String],
    energy_budget: usize,
    input_summary: &str,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    cancel_notify: Arc<tokio::sync::Notify>,
) -> Result<BuiltSubagentSession, String> {
    let collector = Arc::new(CollectorOutput::new());
    let mut context = AgentContext::new();

    let mut prompt = format!(
        "You are a restricted sub-agent. Complete the assigned goal with the available tools, \
         then call `finish_task`.\nParent context summary:\n{}\n\nBe concise.",
        input_summary
    );

    if let Ok(memory) = std::fs::read_to_string("MEMORY.md") {
        prompt.push_str(&format!("\n\nWorkspace Memory:\n{}", memory));
    }
    if let Ok(agents_md) = std::fs::read_to_string("AGENTS.md") {
        prompt.push_str(&format!("\n\nAgent Guidelines:\n{}", agents_md));
    }

    context.system_prompts.push(prompt);
    context.max_history_tokens = 100_000;

    let sub_session_id = format!(
        "sub_{}_{}",
        parent_ctx.session_id,
        uuid::Uuid::new_v4().simple()
    );
    let (telemetry, _handle) = crate::telemetry::TelemetryExporter::new();
    let telemetry = Arc::new(telemetry);
    let task_state_store = Arc::new(crate::task_state::TaskStateStore::new(&sub_session_id));

    let mut tools = filter_subagent_tools(base_tools, allowed_tools, mode);
    if !tools.iter().any(|tool| tool.name() == "task_plan") {
        tools.push(Arc::new(crate::tools::TaskPlanTool::new(
            sub_session_id.clone(),
            task_state_store.clone(),
        )));
    }
    if !tools.iter().any(|tool| tool.name() == "finish_task") {
        tools.push(Arc::new(crate::tools::FinishTaskTool {
            task_state_store: task_state_store.clone(),
        }));
    }

    let mut agent_loop = AgentLoop::new(
        sub_session_id.clone(),
        llm,
        parent_ctx.reply_to.clone(),
        tools,
        context,
        collector.clone() as Arc<dyn crate::core::AgentOutput>,
        telemetry,
        task_state_store,
    );
    agent_loop.set_initial_energy_budget(energy_budget.max(1));
    agent_loop.cancelled = cancelled;
    agent_loop.cancel_token = cancel_notify;

    Ok(BuiltSubagentSession {
        session_id: sub_session_id,
        agent_loop,
        collector,
    })
}

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
    use serde_json::Value;
    use tokio::sync::mpsc;

    struct CaptureOutput;

    #[async_trait]
    impl crate::core::AgentOutput for CaptureOutput {
        async fn on_text(&self, _text: &str) {}
        async fn on_tool_start(&self, _name: &str, _args: &str) {}
        async fn on_tool_end(&self, _result: &str) {}
        async fn on_error(&self, _error: &str) {}
    }

    struct MockTool(&'static str);

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> String {
            self.0.to_string()
        }

        fn description(&self) -> String {
            String::new()
        }

        fn parameters_schema(&self) -> Value {
            serde_json::json!({})
        }

        async fn execute(
            &self,
            _: Value,
            _: &crate::tools::ToolContext,
        ) -> Result<String, crate::tools::ToolError> {
            Ok(String::new())
        }
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

    #[test]
    fn test_filter_subagent_tools_sync_compatible_keeps_explicit_write_tools() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(MockTool("read_file")),
            Arc::new(MockTool("write_file")),
            Arc::new(MockTool("finish_task")),
        ];

        let filtered = filter_subagent_tools(
            &tools,
            &["write_file".to_string()],
            SubagentBuildMode::SyncCompatible,
        );
        let names: Vec<String> = filtered.into_iter().map(|tool| tool.name()).collect();
        assert!(names.contains(&"write_file".to_string()));
        assert!(names.contains(&"finish_task".to_string()));
    }

    #[test]
    fn test_filter_subagent_tools_async_readonly_blocks_explicit_write_tools() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(MockTool("read_file")),
            Arc::new(MockTool("write_file")),
            Arc::new(MockTool("patch_file")),
            Arc::new(MockTool("finish_task")),
        ];

        let filtered = filter_subagent_tools(
            &tools,
            &["write_file".to_string(), "patch_file".to_string()],
            SubagentBuildMode::AsyncReadonly,
        );
        let names: Vec<String> = filtered.into_iter().map(|tool| tool.name()).collect();
        assert!(!names.contains(&"write_file".to_string()));
        assert!(!names.contains(&"patch_file".to_string()));
        assert!(names.contains(&"finish_task".to_string()));
    }
}
