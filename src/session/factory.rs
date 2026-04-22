use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex as AsyncMutex;

use crate::context::AgentContext;
use crate::core::{AgentLoop, AgentOutput};
use crate::event_log::{AgentEvent, EventLog};
use crate::llm_client::LlmClient;
use crate::skills::call_tree::SkillSessionSeed;
use crate::subagent_runtime::{push_recent_debug_event, SubagentDebugEvent, SubagentDebugSnapshot};
use crate::tools::{Tool, ToolContext};

pub struct BuiltSubagentSession {
    pub sub_session_id: String,
    pub transcript_path: String,
    pub event_log_path: String,
    pub agent_loop: AgentLoop,
    pub collector: Arc<CollectorOutput>,
}

pub struct SubagentSessionConfig {
    pub sub_session_id: Option<String>,
    pub allowed_tools: Vec<String>,
    pub restrict_to_allowed_tools: bool,
    pub energy_budget: usize,
    pub timeout_sec: u64,
    pub parent_context_text: String,
    pub skill_session_seed: SkillSessionSeed,
    pub debug: Arc<tokio::sync::RwLock<SubagentDebugSnapshot>>,
    pub cancelled: Arc<std::sync::atomic::AtomicBool>,
    pub cancel_notify: Arc<tokio::sync::Notify>,
    pub allow_subagent_tool: bool,
}

pub struct CollectorOutput {
    session_id: String,
    label: String,
    event_log: EventLog,
    debug: Arc<tokio::sync::RwLock<SubagentDebugSnapshot>>,
    text: AsyncMutex<String>,
    tool_outputs: AsyncMutex<Vec<String>>,
    artifacts: AsyncMutex<Vec<String>>,
}

impl CollectorOutput {
    pub fn new(
        session_id: String,
        label: String,
        debug: Arc<tokio::sync::RwLock<SubagentDebugSnapshot>>,
    ) -> Self {
        Self {
            session_id: session_id.clone(),
            label,
            event_log: EventLog::new(&session_id),
            debug,
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

    async fn append_event(
        &self,
        event_type: &str,
        payload: serde_json::Value,
        text: &str,
        tool_name: Option<&str>,
    ) {
        let mut debug = self.debug.write().await;
        debug.updated_at_unix_ms = crate::subagent_runtime::unix_ms_now();
        push_recent_debug_event(
            &mut debug,
            SubagentDebugEvent {
                kind: event_type.to_string(),
                tool_name: tool_name.map(|value| value.to_string()),
                text: text.to_string(),
                at_unix_ms: crate::subagent_runtime::unix_ms_now(),
            },
        );
        drop(debug);

        let _ = self
            .event_log
            .append(AgentEvent::new(
                event_type,
                self.session_id.clone(),
                None,
                None,
                payload,
            ))
            .await;
    }
}

#[async_trait]
impl crate::core::AgentOutput for CollectorOutput {
    async fn on_text(&self, text: &str) {
        if !text.trim().is_empty() {
            let mut debug = self.debug.write().await;
            debug.last_model_text = Some(crate::subagent_runtime::truncate_debug_text(text, 500));
            debug.updated_at_unix_ms = crate::subagent_runtime::unix_ms_now();
        }
        self.text.lock().await.push_str(text);
    }

    async fn on_thinking(&self, text: &str) {
        if !text.trim().is_empty() {
            let mut debug = self.debug.write().await;
            debug.last_thought_text = Some(crate::subagent_runtime::truncate_debug_text(text, 500));
            debug.updated_at_unix_ms = crate::subagent_runtime::unix_ms_now();
        }
    }

    async fn on_tool_start(&self, name: &str, args: &str) {
        tracing::debug!(target: "subagent", "[Sub:{}] → {} {}", self.label, name, crate::context::AgentContext::truncate_chars(args, 200));
        {
            let mut debug = self.debug.write().await;
            debug.step_count += 1;
            debug.last_tool_name = Some(name.to_string());
            debug.last_tool_args_summary =
                Some(crate::subagent_runtime::truncate_debug_text(args, 500));
            debug.updated_at_unix_ms = crate::subagent_runtime::unix_ms_now();
        }
        self.append_event(
            "subagent_tool_start",
            serde_json::json!({
                "tool_name": name,
                "args": crate::subagent_runtime::truncate_debug_text(args, 500),
            }),
            &format!(
                "{} {}",
                name,
                crate::subagent_runtime::truncate_debug_text(args, 120)
            ),
            Some(name),
        )
        .await;
        if name == "write_file" || name == "patch_file" {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(args) {
                if let Some(path) = parsed.get("path").and_then(|p| p.as_str()) {
                    self.artifacts.lock().await.push(path.to_string());
                }
            }
        }
    }

    async fn on_tool_end(&self, result: &str) {
        tracing::debug!(target: "subagent", "[Sub:{}] ← {}", self.label, crate::context::AgentContext::truncate_chars(result, 200));
        {
            let mut debug = self.debug.write().await;
            debug.last_tool_result_summary =
                Some(crate::subagent_runtime::truncate_debug_text(result, 500));
            debug.updated_at_unix_ms = crate::subagent_runtime::unix_ms_now();
        }
        let current_tool = self.debug.read().await.last_tool_name.clone();
        self.append_event(
            "subagent_tool_end",
            json!({
                "tool_name": current_tool,
                "result": crate::subagent_runtime::truncate_debug_text(result, 500),
            }),
            &crate::subagent_runtime::truncate_debug_text(result, 160),
            current_tool.as_deref(),
        )
        .await;
        let truncated = if result.len() > 500 {
            format!(
                "{}...(truncated)",
                crate::context::AgentContext::truncate_chars(result, 500)
            )
        } else {
            result.to_string()
        };
        self.tool_outputs.lock().await.push(truncated);
    }

    async fn on_error(&self, error: &str) {
        tracing::warn!(target: "subagent", "[Sub:{}] ✗ {}", self.label, error);
        {
            let mut debug = self.debug.write().await;
            debug.last_error = Some(crate::subagent_runtime::truncate_debug_text(error, 500));
            debug.updated_at_unix_ms = crate::subagent_runtime::unix_ms_now();
        }
        let current_tool = self.debug.read().await.last_tool_name.clone();
        self.append_event(
            "subagent_error",
            json!({
                "tool_name": current_tool,
                "error": crate::subagent_runtime::truncate_debug_text(error, 500),
            }),
            &crate::subagent_runtime::truncate_debug_text(error, 160),
            current_tool.as_deref(),
        )
        .await;
        self.text
            .lock()
            .await
            .push_str(&format!("[ERROR] {}\n", error));
    }

    async fn on_llm_request(&self, prompt_summary: &str) {
        self.append_event(
            "llm_request",
            serde_json::json!({ "summary": prompt_summary }),
            &format!("LLM Request: {}", prompt_summary),
            None,
        )
        .await;
    }

    async fn on_llm_response(&self, response_summary: &str) {
        self.append_event(
            "llm_response",
            serde_json::json!({ "summary": crate::subagent_runtime::truncate_debug_text(response_summary, 500) }),
            &format!("LLM Response: {}", crate::subagent_runtime::truncate_debug_text(response_summary, 120)),
            None,
        ).await;
    }
}

pub fn filter_subagent_tools(
    base_tools: &[Arc<dyn Tool>],
    allowed: &[String],
    restrict_to_allowed_tools: bool,
    allow_subagent_tool: bool,
) -> Vec<Arc<dyn Tool>> {
    let runtime_tools = ["finish_task", "task_plan"];
    let code_mode_companion_allowed = allowed.iter().any(|tool| tool == "exec");
    let mut accepted = Vec::new();

    for tool in base_tools {
        let name = tool.name();
        if name == "ask_user_question" {
            continue;
        }
        if name == "subagent" && !allow_subagent_tool {
            continue;
        }

        if !restrict_to_allowed_tools
            || runtime_tools.contains(&name.as_str())
            || allowed.contains(&name)
            || (name == "wait" && code_mode_companion_allowed)
        {
            accepted.push(tool.clone());
        }
    }

    accepted
}

pub fn build_subagent_session(
    parent_ctx: &ToolContext,
    llm: Arc<dyn LlmClient>,
    base_tools: &[Arc<dyn Tool>],
    config: SubagentSessionConfig,
) -> Result<BuiltSubagentSession, String> {
    let SubagentSessionConfig {
        sub_session_id,
        allowed_tools,
        restrict_to_allowed_tools,
        energy_budget,
        timeout_sec,
        parent_context_text,
        skill_session_seed,
        debug,
        cancelled,
        cancel_notify,
        allow_subagent_tool,
    } = config;

    let sub_session_id = sub_session_id.unwrap_or_else(|| {
        format!(
            "sub_{}_{}",
            parent_ctx.session_id,
            uuid::Uuid::new_v4().simple()
        )
    });

    let label = if sub_session_id.len() > 8 {
        sub_session_id[sub_session_id.len() - 8..].to_string()
    } else {
        sub_session_id.clone()
    };

    let collector = Arc::new(CollectorOutput::new(sub_session_id.clone(), label, debug));

    let session_dir = crate::schema::StoragePaths::session_dir(&sub_session_id);
    let _ = std::fs::create_dir_all(&session_dir);
    let transcript_path = session_dir.join("transcript.json");
    let mut context = AgentContext::new().with_transcript_path(transcript_path);

    let mut prompt = "You are a delegated sub-agent. Complete the assigned goal with the available tools, then call `finish_task`.".to_string();
    if !parent_context_text.trim().is_empty() {
        prompt.push_str(&format!(
            "\nParent context:\n{}",
            parent_context_text.trim()
        ));
    }
    prompt.push_str(
        "\n\nDelegated sub-agents must not ask the user questions directly.\nBe concise.",
    );

    if let Ok(memory) = std::fs::read_to_string("MEMORY.md") {
        prompt.push_str(&format!("\n\nWorkspace Memory:\n{}", memory));
    }
    if let Ok(agents_md) = std::fs::read_to_string("AGENTS.md") {
        prompt.push_str(&format!("\n\nAgent Guidelines:\n{}", agents_md));
    }

    context.system_prompts.push(prompt);
    context.max_history_tokens = 100_000;

    let (telemetry, _handle) = crate::telemetry::TelemetryExporter::new();
    let telemetry = Arc::new(telemetry);
    let task_state_store = Arc::new(crate::task_state::TaskStateStore::new(&sub_session_id));

    let mut tools = filter_subagent_tools(
        base_tools,
        &allowed_tools,
        restrict_to_allowed_tools,
        allow_subagent_tool,
    );
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
    agent_loop.set_session_timeout(Duration::from_secs(timeout_sec.max(1)));
    agent_loop.is_subagent = true;
    if let Some(trace) = &parent_ctx.trace {
        agent_loop.set_trace_seed(crate::trace::TraceSeed {
            trace_id: trace.trace_id.clone(),
            run_id: trace.run_id.clone(),
            root_session_id: trace.root_session_id.clone(),
            task_id: trace.task_id.clone(),
            parent_span_id: trace.parent_span_id.clone(),
        });
    }
    agent_loop.add_extension(Arc::new(
        crate::skills::runtime::SkillRuntime::with_session_seed(
            sub_session_id.clone(),
            skill_session_seed,
        ),
    ));
    agent_loop.cancelled = cancelled;
    agent_loop.cancel_token = cancel_notify;

    Ok(BuiltSubagentSession {
        sub_session_id: sub_session_id.clone(),
        transcript_path: crate::schema::StoragePaths::session_transcript_file(&sub_session_id)
            .display()
            .to_string(),
        event_log_path: crate::schema::StoragePaths::events_file(&sub_session_id)
            .display()
            .to_string(),
        agent_loop,
        collector,
    })
}

pub fn build_agent_session(
    session_id: &str,
    reply_to: &str,
    llm: Arc<dyn LlmClient>,
    tools: Vec<Arc<dyn Tool>>,
    subagent_runtime: crate::subagent_runtime::SubagentRuntime,
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
    session_tools.push(Arc::new(crate::tools::SubagentTool::new(
        llm.clone(),
        subagent_base_tools.clone(),
        subagent_runtime.clone(),
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
    agent_loop.add_extension(Arc::new(
        crate::skills::runtime::SkillRuntime::new_for_session(session_id.to_string()),
    ));
    agent_loop.add_extension(Arc::new(
        crate::subagent_notification::SubagentNotificationExtension::new(
            session_id,
            subagent_runtime.clone(),
        ),
    ));

    // ── Sandbox extension ──
    {
        let config = crate::config::AppConfig::load();
        let sandbox_config = config.sandbox.unwrap_or_default();
        let level = sandbox_config.parsed_level();
        if level != crate::tools::sandbox::SandboxLevel::Unrestricted {
            let work_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let policy = sandbox_config.build_default_policy(&work_dir);
            let enforcer = crate::tools::sandbox::SandboxEnforcer::detect(policy);

            if sandbox_config.require_os_sandbox.unwrap_or(false) && !enforcer.is_available() {
                let hint = if cfg!(target_os = "linux") {
                    "Install with: apt install bubblewrap"
                } else if cfg!(target_os = "macos") {
                    "sandbox-exec should be present at /usr/bin/sandbox-exec on macOS"
                } else {
                    "OS-level sandboxing is not supported on this platform"
                };
                return Err(format!(
                    "Sandbox: OS-level isolation is required but not available. {hint}"
                ));
            }

            agent_loop.add_extension(Arc::new(crate::sandbox_extension::SandboxExtension::new(
                std::sync::Arc::new(enforcer),
            )));
            tracing::info!("Sandbox extension registered (level={:?})", level);
        }
    }

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
        pub last_tools: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl crate::llm_client::LlmClient for InspectingLlm {
        fn model_name(&self) -> &str {
            "inspecting"
        }

        fn provider_name(&self) -> &str {
            "test-provider"
        }

        fn capabilities(&self) -> crate::llm_client::LlmCapabilities {
            crate::llm_client::LlmCapabilities {
                function_tools: true,
                custom_tools: false,
                parallel_tool_calls: true,
                supports_code_mode: true,
            }
        }

        async fn stream(
            &self,
            _messages: Vec<crate::context::Message>,
            system_instruction: Option<crate::context::Message>,
            tools: Vec<Arc<dyn Tool>>,
        ) -> Result<mpsc::Receiver<crate::llm_client::StreamEvent>, crate::llm_client::LlmError>
        {
            let text = system_instruction
                .and_then(|message| message.parts.into_iter().find_map(|part| part.text))
                .unwrap_or_default();
            *self.last_system.lock().unwrap() = Some(text);
            *self.last_tools.lock().unwrap() = tools.into_iter().map(|tool| tool.name()).collect();

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
            last_tools: std::sync::Mutex::new(Vec::new()),
        });
        let output = Arc::new(CaptureOutput);
        let agent = build_agent_session(
            session_id,
            "cli",
            llm.clone(),
            Vec::new(),
            crate::subagent_runtime::SubagentRuntime::new(llm.clone(), Vec::new(), 2),
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

    #[tokio::test]
    async fn test_build_subagent_session_excludes_interactive_tools_for_default_subagents() {
        let session_id = "test-subagent-noninteractive";
        cleanup_session(session_id);
        let llm = Arc::new(InspectingLlm {
            last_system: std::sync::Mutex::new(None),
            last_tools: std::sync::Mutex::new(Vec::new()),
        });
        let parent_ctx = crate::tools::ToolContext::new(session_id, "cli");
        let base_tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(MockTool("read_file")),
            Arc::new(MockTool("ask_user_question")),
        ];
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_notify = Arc::new(tokio::sync::Notify::new());

        let built = build_subagent_session(
            &parent_ctx,
            llm.clone(),
            &base_tools,
            SubagentSessionConfig {
                sub_session_id: Some(format!("sub_{session_id}")),
                allowed_tools: Vec::new(),
                restrict_to_allowed_tools: false,
                energy_budget: 3,
                timeout_sec: 3,
                parent_context_text: "repo context".to_string(),
                skill_session_seed: SkillSessionSeed::default(),
                debug: Arc::new(tokio::sync::RwLock::new(SubagentDebugSnapshot::default())),
                cancelled,
                cancel_notify,
                allow_subagent_tool: false,
            },
        )
        .expect("subagent session should build");

        let mut agent = built.agent_loop;
        let _ = agent.step("inspect".to_string()).await.unwrap();

        let tool_names = llm.last_tools.lock().unwrap().clone();
        assert!(tool_names.contains(&"read_file".to_string()));
        assert!(!tool_names.contains(&"ask_user_question".to_string()));
        assert!(!tool_names.contains(&"subagent".to_string()));

        cleanup_session(session_id);
    }

    #[test]
    fn test_filter_subagent_tools_defaults_to_all_non_recursive_tools() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(MockTool("read_file")),
            Arc::new(MockTool("web_fetch")),
            Arc::new(MockTool("write_file")),
            Arc::new(MockTool("execute_bash")),
            Arc::new(MockTool("ask_user_question")),
            Arc::new(MockTool("subagent")),
            Arc::new(MockTool("finish_task")),
        ];

        let filtered = filter_subagent_tools(&tools, &[], false, false);
        let names: Vec<String> = filtered.into_iter().map(|tool| tool.name()).collect();
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"web_fetch".to_string()));
        assert!(names.contains(&"write_file".to_string()));
        assert!(names.contains(&"execute_bash".to_string()));
        assert!(names.contains(&"finish_task".to_string()));
        assert!(!names.contains(&"ask_user_question".to_string()));
        assert!(!names.contains(&"subagent".to_string()));
    }

    #[test]
    fn test_filter_subagent_tools_honors_explicit_whitelist() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(MockTool("read_file")),
            Arc::new(MockTool("write_file")),
            Arc::new(MockTool("finish_task")),
        ];

        let filtered = filter_subagent_tools(&tools, &["write_file".to_string()], true, false);
        let names: Vec<String> = filtered.into_iter().map(|tool| tool.name()).collect();
        assert!(names.contains(&"write_file".to_string()));
        assert!(names.contains(&"finish_task".to_string()));
        assert!(!names.contains(&"read_file".to_string()));
    }

    #[test]
    fn test_filter_subagent_tools_keeps_wait_when_exec_is_whitelisted() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(MockTool("exec")),
            Arc::new(MockTool("wait")),
            Arc::new(MockTool("read_file")),
        ];

        let filtered = filter_subagent_tools(&tools, &["exec".to_string()], true, false);
        let names: Vec<String> = filtered.into_iter().map(|tool| tool.name()).collect();
        assert!(names.contains(&"exec".to_string()));
        assert!(names.contains(&"wait".to_string()));
        assert!(!names.contains(&"read_file".to_string()));
    }

    #[test]
    fn test_filter_subagent_tools_whitelist_keeps_runtime_tools() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(MockTool("read_file")),
            Arc::new(MockTool("subagent")),
            Arc::new(MockTool("finish_task")),
            Arc::new(MockTool("task_plan")),
        ];

        let filtered = filter_subagent_tools(&tools, &["subagent".to_string()], true, true);
        let names: Vec<String> = filtered.into_iter().map(|tool| tool.name()).collect();
        assert!(names.contains(&"subagent".to_string()));
        assert!(names.contains(&"finish_task".to_string()));
        assert!(names.contains(&"task_plan".to_string()));
        assert!(!names.contains(&"read_file".to_string()));
    }

    #[test]
    fn test_filter_subagent_tools_can_opt_in_to_subagent_for_skill_sessions() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(MockTool("read_file")),
            Arc::new(MockTool("subagent")),
            Arc::new(MockTool("finish_task")),
        ];

        let filtered = filter_subagent_tools(&tools, &["subagent".to_string()], true, true);
        let names: Vec<String> = filtered.into_iter().map(|tool| tool.name()).collect();
        assert!(names.contains(&"subagent".to_string()));
        assert!(names.contains(&"finish_task".to_string()));
        assert!(!names.contains(&"read_file".to_string()));
    }
}
