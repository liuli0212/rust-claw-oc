use super::*;
use crate::context::Message;
use crate::llm_client::{LlmCapabilities, LlmClient, LlmError, StreamEvent};
use crate::tools::protocol::ToolExecutionEnvelope;
use crate::tools::Tool;
use crate::trace::TraceKind;
use async_trait::async_trait;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

struct TestLlmClient {
    stream_calls: AtomicUsize,
    stream_delay_ms: u64,
}

impl TestLlmClient {
    fn new() -> Self {
        Self {
            stream_calls: AtomicUsize::new(0),
            stream_delay_ms: 0,
        }
    }

    fn new_with_delay(stream_delay_ms: u64) -> Self {
        Self {
            stream_calls: AtomicUsize::new(0),
            stream_delay_ms,
        }
    }

    fn stream_call_count(&self) -> usize {
        self.stream_calls.load(Ordering::SeqCst)
    }
}

struct PromptCapturingLlm {
    last_system: Mutex<Option<String>>,
    events: Mutex<Vec<StreamEvent>>,
    capabilities: LlmCapabilities,
}

impl PromptCapturingLlm {
    fn new(events: Vec<StreamEvent>) -> Self {
        Self::new_with_capabilities(
            events,
            LlmCapabilities {
                function_tools: true,
                custom_tools: false,
                parallel_tool_calls: true,
                supports_code_mode: true,
            },
        )
    }

    fn new_with_capabilities(events: Vec<StreamEvent>, capabilities: LlmCapabilities) -> Self {
        Self {
            last_system: Mutex::new(None),
            events: Mutex::new(events),
            capabilities,
        }
    }

    #[allow(dead_code)]
    fn last_system_text(&self) -> Option<String> {
        self.last_system.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmClient for TestLlmClient {
    fn model_name(&self) -> &str {
        "test-model"
    }

    fn provider_name(&self) -> &str {
        "test-provider"
    }

    fn capabilities(&self) -> LlmCapabilities {
        LlmCapabilities {
            function_tools: true,
            custom_tools: false,
            parallel_tool_calls: true,
            supports_code_mode: true,
        }
    }

    async fn stream(
        &self,
        _messages: Vec<Message>,
        _system_instruction: Option<Message>,
        _tools: Vec<Arc<dyn Tool>>,
    ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
        if self.stream_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.stream_delay_ms)).await;
        }
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        let (_tx, rx) = mpsc::channel(1);
        Ok(rx)
    }
}

#[async_trait]
impl LlmClient for PromptCapturingLlm {
    fn model_name(&self) -> &str {
        "prompt-capturing"
    }

    fn provider_name(&self) -> &str {
        "test-provider"
    }

    fn capabilities(&self) -> LlmCapabilities {
        self.capabilities
    }

    async fn stream(
        &self,
        _messages: Vec<Message>,
        system_instruction: Option<Message>,
        _tools: Vec<Arc<dyn Tool>>,
    ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
        let text = system_instruction
            .and_then(|message| message.parts.into_iter().find_map(|part| part.text))
            .unwrap_or_default();
        *self.last_system.lock().unwrap() = Some(text);

        let (tx, rx) = mpsc::channel(8);
        let events = std::mem::take(&mut *self.events.lock().unwrap());
        tokio::spawn(async move {
            for event in events {
                let _ = tx.send(event).await;
            }
            let _ = tx.send(StreamEvent::Done).await;
        });
        Ok(rx)
    }
}

#[derive(Default)]
struct OutputState {
    text: String,
    thinking: String,
}

struct TestOutput {
    state: Mutex<OutputState>,
}

impl TestOutput {
    fn new() -> Self {
        Self {
            state: Mutex::new(OutputState::default()),
        }
    }

    fn snapshot(&self) -> (String, String) {
        let state = self.state.lock().unwrap();
        (state.text.clone(), state.thinking.clone())
    }
}

#[async_trait]
impl AgentOutput for TestOutput {
    async fn on_text(&self, text: &str) {
        self.state.lock().unwrap().text.push_str(text);
    }

    async fn on_thinking(&self, text: &str) {
        self.state.lock().unwrap().thinking.push_str(text);
    }

    async fn on_tool_start(&self, _name: &str, _args: &str) {}

    async fn on_tool_end(&self, _result: &str) {}

    async fn on_error(&self, _error: &str) {}
}

fn cleanup_session(session_id: &str) {
    let session_dir = crate::schema::StoragePaths::session_dir(session_id);
    let _ = std::fs::remove_dir_all(session_dir);
}

fn make_agent_loop(
    output: Arc<TestOutput>,
    llm: Arc<TestLlmClient>,
    session_id: &str,
) -> AgentLoop {
    let (telemetry, _handle) = crate::telemetry::TelemetryExporter::new();
    AgentLoop::new(
        session_id.to_string(),
        llm,
        "test_cli".to_string(), // Add reply_to
        Vec::new(),
        AgentContext::new(),
        output,
        Arc::new(telemetry),
        Arc::new(crate::task_state::TaskStateStore::new(session_id)),
    )
}

#[derive(Clone)]
struct ContextCapturingTool {
    contexts: Arc<Mutex<Vec<crate::tools::ToolContext>>>,
}

#[async_trait]
impl Tool for ContextCapturingTool {
    fn name(&self) -> String {
        "echo_tool".to_string()
    }

    fn description(&self) -> String {
        "echo".to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({})
    }

    fn has_side_effects(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &crate::tools::ToolContext,
    ) -> Result<String, crate::tools::ToolError> {
        self.contexts.lock().unwrap().push(ctx.clone());
        Ok(json!({
            "value": args.get("value").and_then(serde_json::Value::as_str).unwrap_or_default()
        })
        .to_string())
    }
}

struct MutatingTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for MutatingTool {
    fn name(&self) -> String {
        "mutating_tool".to_string()
    }

    fn description(&self) -> String {
        "mutate".to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({})
    }

    fn has_side_effects(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &crate::tools::ToolContext,
    ) -> Result<String, crate::tools::ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({ "ok": true }).to_string())
    }
}

struct ContextTaggingExtension;

#[async_trait]
impl super::extensions::ExecutionExtension for ContextTaggingExtension {
    async fn before_turn_start(&self, _input: &str) -> super::extensions::ExtensionDecision {
        super::extensions::ExtensionDecision::Continue
    }

    async fn before_prompt_build(
        &self,
        draft: super::extensions::PromptDraft,
    ) -> super::extensions::PromptDraft {
        draft
    }

    async fn before_tool_resolution(&self, tools: Vec<Arc<dyn Tool>>) -> Vec<Arc<dyn Tool>> {
        tools
    }

    async fn enrich_tool_context(
        &self,
        mut ctx: crate::tools::ToolContext,
    ) -> crate::tools::ToolContext {
        ctx.active_skill_name = Some("code-mode-test-skill".to_string());
        ctx
    }

    async fn after_tool_result(&self, _result: &ToolExecutionEnvelope) {}

    async fn before_finish(&self) -> super::extensions::FinishDecision {
        super::extensions::FinishDecision::Allow
    }
}

async fn collect_trace_records_for_session(
    rx: &mut tokio::sync::broadcast::Receiver<crate::trace::TraceRecord>,
    session_id: &str,
    target: usize,
) -> Vec<crate::trace::TraceRecord> {
    let mut records = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);

    while tokio::time::Instant::now() < deadline && records.len() < target {
        match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            Ok(Ok(record)) if record.session_id == session_id => records.push(record),
            Ok(Ok(_)) | Ok(Err(_)) => continue,
            Err(_) => break,
        }
    }

    records
}

#[test]
fn test_run_exit_label_matches_public_status_names() {
    assert_eq!(RunExit::Finished("done".to_string()).label(), "finished");
    assert_eq!(RunExit::StoppedByUser.label(), "stopped_by_user");
    assert_eq!(RunExit::YieldedToUser.label(), "yielded_to_user");
    assert_eq!(
        RunExit::RecoverableFailed("retry".to_string()).label(),
        "recoverable_failed"
    );
    assert_eq!(
        RunExit::CriticallyFailed("boom".to_string()).label(),
        "critically_failed"
    );
    assert_eq!(
        RunExit::AutopilotStalled("stuck".to_string()).label(),
        "autopilot_stalled"
    );
}

#[test]
fn test_strip_think_blocks_removes_closed_and_unclosed_blocks() {
    assert_eq!(
        AgentLoop::strip_think_blocks("hello<think>secret</think>world"),
        "helloworld"
    );
    assert_eq!(
        AgentLoop::strip_think_blocks("visible<think>hidden forever"),
        "visible"
    );
}

#[test]
fn test_is_transient_llm_error_matches_retryable_signals_only() {
    assert!(AgentLoop::is_transient_llm_error(&LlmError::ApiError(
        "HTTP 503 upstream timeout".to_string()
    )));
    assert!(AgentLoop::is_transient_llm_error(&LlmError::ApiError(
        "connection reset by peer".to_string()
    )));
    assert!(!AgentLoop::is_transient_llm_error(&LlmError::ApiError(
        "invalid API key".to_string()
    )));
}

#[tokio::test]
async fn test_process_streaming_text_routes_visible_and_thinking_segments() {
    let output = Arc::new(TestOutput::new());
    let llm = Arc::new(TestLlmClient::new());
    let session_id = "test-streaming-text";
    cleanup_session(session_id);
    let agent = make_agent_loop(output.clone(), llm, session_id);
    let mut processed_idx = 0;
    let mut in_think_block = false;
    let mut full_text = "Visible <think>internal".to_string();

    agent
        .process_streaming_text(&full_text, &mut processed_idx, &mut in_think_block)
        .await;

    full_text.push_str(" reasoning</think> done <final>answer</final>");
    agent
        .process_streaming_text(&full_text, &mut processed_idx, &mut in_think_block)
        .await;

    let (text, thinking) = output.snapshot();
    assert_eq!(text, "Visible  done answer");
    assert_eq!(thinking, "internal reasoning");
    assert_eq!(processed_idx, full_text.len());
    assert!(!in_think_block);
    cleanup_session(session_id);
}

#[tokio::test]
async fn test_step_with_empty_goal_yields_without_starting_turn_or_llm() {
    let output = Arc::new(TestOutput::new());
    let llm = Arc::new(TestLlmClient::new());
    let session_id = "test-empty-goal";
    cleanup_session(session_id);
    let mut agent = make_agent_loop(output, llm.clone(), session_id);

    let exit = agent.step("   ".to_string()).await.unwrap();

    assert_eq!(exit, RunExit::YieldedToUser);
    assert!(agent.context.current_turn.is_none());
    assert_eq!(llm.stream_call_count(), 0);
    assert!(!crate::schema::StoragePaths::task_state_file(session_id).exists());
    cleanup_session(session_id);
}

#[tokio::test]
async fn test_step_honors_cancel_during_pending_llm_stream_start() {
    let output = Arc::new(TestOutput::new());
    let llm = Arc::new(TestLlmClient::new_with_delay(200));
    let session_id = "test-cancel-before-stream";
    cleanup_session(session_id);
    let mut agent = make_agent_loop(output, llm.clone(), session_id);
    let cancel_token = agent.cancel_token.clone();
    let cancelled = agent.cancelled.clone();

    let step_handle =
        tokio::spawn(async move { agent.step("Refactor the core loop".to_string()).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    cancelled.store(true, Ordering::SeqCst);
    cancel_token.notify_waiters();

    let exit = step_handle.await.unwrap().unwrap();
    let store = crate::task_state::TaskStateStore::new(session_id);
    let stored_state = store.load().unwrap();

    assert_eq!(exit, RunExit::StoppedByUser);
    assert_eq!(llm.stream_call_count(), 0);
    assert_eq!(stored_state.status, "in_progress");
    assert_eq!(stored_state.goal.as_deref(), Some("Refactor the core loop"));
    cleanup_session(session_id);
}

#[tokio::test]
async fn test_step_yields_after_ask_user_tool_result() {
    struct AskUserTool;

    #[async_trait]
    impl Tool for AskUserTool {
        fn name(&self) -> String {
            "ask_user_question".to_string()
        }

        fn description(&self) -> String {
            "ask".to_string()
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }

        fn has_side_effects(&self) -> bool {
            false
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &crate::tools::ToolContext,
        ) -> Result<String, crate::tools::ToolError> {
            crate::tools::protocol::StructuredToolOutput::new(
                "ask_user_question",
                true,
                "waiting".to_string(),
                None,
                None,
                false,
            )
            .with_await_user(crate::tools::protocol::UserPromptRequest {
                question: "What is your goal?".to_string(),
                context_key: "goal".to_string(),
                options: vec!["A".to_string(), "B".to_string()],
                recommendation: Some("A".to_string()),
            })
            .to_json_string()
        }
    }

    let llm = Arc::new(PromptCapturingLlm::new(vec![StreamEvent::ToolCall(
        crate::context::FunctionCall {
            name: "ask_user_question".to_string(),
            args: serde_json::json!({
                "question": "What is your goal?",
                "context_key": "goal",
                "options": ["A", "B"],
                "recommendation": "A"
            }),
            id: Some("call_1".to_string()),
        },
        None,
    )]));
    let output = Arc::new(TestOutput::new());
    let session_id = "test-ask-user-yield";
    cleanup_session(session_id);

    let (telemetry, _handle) = crate::telemetry::TelemetryExporter::new();
    let mut agent = AgentLoop::new(
        session_id.to_string(),
        llm,
        "test_cli".to_string(),
        vec![Arc::new(AskUserTool)],
        AgentContext::new(),
        output.clone(),
        Arc::new(telemetry),
        Arc::new(crate::task_state::TaskStateStore::new(session_id)),
    );

    let exit = agent
        .step("Help me choose a goal".to_string())
        .await
        .unwrap();
    let (text, _) = output.snapshot();

    assert_eq!(exit, RunExit::YieldedToUser);
    assert!(text.contains("What is your goal?"));
    cleanup_session(session_id);
}

#[tokio::test]
async fn test_code_mode_notice_is_added_when_exec_is_visible() {
    let llm = Arc::new(PromptCapturingLlm::new(vec![StreamEvent::Text(
        "Ready".to_string(),
    )]));
    let output = Arc::new(TestOutput::new());
    let session_id = "test-code-mode-notice";
    cleanup_session(session_id);

    let (telemetry, _handle) = crate::telemetry::TelemetryExporter::new();
    let task_state_store = Arc::new(crate::task_state::TaskStateStore::new(session_id));
    let mut agent = AgentLoop::new(
        session_id.to_string(),
        llm.clone(),
        "test_cli".to_string(),
        vec![Arc::new(crate::tools::ExecTool)],
        AgentContext::new(),
        output,
        Arc::new(telemetry),
        task_state_store,
    );

    let exit = agent.step("Use code mode".to_string()).await.unwrap();
    assert_eq!(exit, RunExit::YieldedToUser);

    let system_text = llm.last_system_text().unwrap_or_default();
    assert!(system_text.contains("Code Mode is enabled for this provider."));
    assert!(system_text.contains("prefer the `exec` tool"));

    cleanup_session(session_id);
}

#[tokio::test]
async fn test_code_mode_notice_is_omitted_when_provider_disables_it() {
    let llm = Arc::new(PromptCapturingLlm::new_with_capabilities(
        vec![StreamEvent::Text("Ready".to_string())],
        LlmCapabilities {
            function_tools: true,
            custom_tools: false,
            parallel_tool_calls: true,
            supports_code_mode: false,
        },
    ));
    let output = Arc::new(TestOutput::new());
    let session_id = "test-code-mode-disabled";
    cleanup_session(session_id);

    let (telemetry, _handle) = crate::telemetry::TelemetryExporter::new();
    let task_state_store = Arc::new(crate::task_state::TaskStateStore::new(session_id));
    let mut agent = AgentLoop::new(
        session_id.to_string(),
        llm.clone(),
        "test_cli".to_string(),
        vec![Arc::new(crate::tools::ExecTool)],
        AgentContext::new(),
        output,
        Arc::new(telemetry),
        task_state_store,
    );

    let exit = agent
        .step("Code mode should stay hidden".to_string())
        .await
        .unwrap();
    assert_eq!(exit, RunExit::YieldedToUser);

    let system_text = llm.last_system_text().unwrap_or_default();
    assert!(!system_text.contains("Code Mode is enabled for this provider."));

    cleanup_session(session_id);
}

#[tokio::test]
async fn test_code_mode_nested_tool_context_is_enriched_by_extensions() {
    let output = Arc::new(TestOutput::new());
    let llm = Arc::new(TestLlmClient::new());
    let session_id = "test-code-mode-extension-context";
    cleanup_session(session_id);

    let captured_contexts = Arc::new(Mutex::new(Vec::new()));
    let echo_tool = Arc::new(ContextCapturingTool {
        contexts: captured_contexts.clone(),
    });
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(crate::tools::ExecTool), echo_tool];

    let (telemetry, _handle) = crate::telemetry::TelemetryExporter::new();
    let mut agent = AgentLoop::new(
        session_id.to_string(),
        llm,
        "test_cli".to_string(),
        tools.clone(),
        AgentContext::new(),
        output,
        Arc::new(telemetry),
        Arc::new(crate::task_state::TaskStateStore::new(session_id)),
    );
    agent.add_extension(Arc::new(ContextTaggingExtension));
    agent.begin_trace_run("exercise code mode", None);

    let outcome = agent
        .dispatch_tool_call(
            &crate::context::FunctionCall {
                name: "exec".to_string(),
                args: json!({
                    "code": r#"
const response = await tools.echo_tool({ value: "hello" });
text(response.value);
"#
                }),
                id: Some("call_exec_ctx".to_string()),
            },
            &tools,
            5,
            agent.trace_context_with_parent(agent.turn_span_id(), Some(1)),
        )
        .await;
    agent.finish_active_trace("run_finished", TraceStatus::Ok, None);

    assert!(!outcome.is_error, "{}", outcome.result);

    let captured_contexts = captured_contexts.lock().unwrap().clone();
    assert_eq!(captured_contexts.len(), 1);
    assert_eq!(
        captured_contexts[0].active_skill_name.as_deref(),
        Some("code-mode-test-skill")
    );
    assert!(captured_contexts[0]
        .trace
        .as_ref()
        .and_then(|trace| trace.parent_span_id.clone())
        .is_some());
    assert!(captured_contexts[0]
        .visible_tools
        .iter()
        .any(|tool_name| tool_name == "echo_tool"));

    cleanup_session(session_id);
}

#[test]
fn execution_guard_state_escalates_after_repeated_failures() {
    let mut guard_state = ExecutionGuardState::default();
    let args = json!({ "path": "foo.rs" });

    assert_eq!(
        guard_state.record_action_outcome("write_file", &args, true),
        None
    );
    assert_eq!(
        guard_state.record_action_outcome("write_file", &args, true),
        None
    );
    assert_eq!(
        guard_state.record_action_outcome("write_file", &args, true),
        Some(ExecutionGuardSignal::ReflectionWarning)
    );

    assert_eq!(
        guard_state.record_action_outcome("write_file", &args, true),
        None
    );
    assert_eq!(
        guard_state.record_action_outcome("write_file", &args, true),
        None
    );
    assert_eq!(
        guard_state.record_action_outcome("write_file", &args, true),
        Some(ExecutionGuardSignal::AutopilotMeltdown)
    );
}

#[tokio::test]
async fn test_code_mode_nested_tool_respects_autopilot_denials() {
    let output = Arc::new(TestOutput::new());
    let llm = Arc::new(TestLlmClient::new());
    let session_id = "test-code-mode-autopilot-denial";
    cleanup_session(session_id);

    let calls = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(crate::tools::ExecTool),
        Arc::new(MutatingTool {
            calls: calls.clone(),
        }),
    ];

    let (telemetry, _handle) = crate::telemetry::TelemetryExporter::new();
    let mut agent = AgentLoop::new(
        session_id.to_string(),
        llm,
        "test_cli".to_string(),
        tools.clone(),
        AgentContext::new(),
        output,
        Arc::new(telemetry),
        Arc::new(crate::task_state::TaskStateStore::new(session_id)),
    );
    agent.enable_autopilot();
    agent.begin_trace_run("autopilot code mode", None);

    let outcome = agent
        .dispatch_tool_call(
            &crate::context::FunctionCall {
                name: "exec".to_string(),
                args: json!({
                    "code": r#"
await tools.mutating_tool({ path: "unsafe.txt" });
"#
                }),
                id: Some("call_exec_autopilot".to_string()),
            },
            &tools,
            5,
            agent.trace_context_with_parent(agent.turn_span_id(), Some(1)),
        )
        .await;
    agent.finish_active_trace("run_finished", TraceStatus::Ok, None);

    assert!(outcome.is_error);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let envelope = ToolExecutionEnvelope::from_json_str(&outcome.result).expect("exec envelope");
    assert!(
        envelope.result.output.contains("Action Denied")
            || outcome.result.contains("Action Denied"),
        "{}",
        outcome.result
    );

    cleanup_session(session_id);
}

#[tokio::test]
async fn test_code_mode_trace_events_include_exec_metadata() {
    let output = Arc::new(TestOutput::new());
    let llm = Arc::new(TestLlmClient::new());
    let session_id = "test-code-mode-trace-metadata";
    cleanup_session(session_id);

    let captured_contexts = Arc::new(Mutex::new(Vec::new()));
    let echo_tool = Arc::new(ContextCapturingTool {
        contexts: captured_contexts,
    });
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(crate::tools::ExecTool), echo_tool];

    let (telemetry, _handle) = crate::telemetry::TelemetryExporter::new();
    let mut agent = AgentLoop::new(
        session_id.to_string(),
        llm,
        "test_cli".to_string(),
        tools.clone(),
        AgentContext::new(),
        output,
        Arc::new(telemetry),
        Arc::new(crate::task_state::TaskStateStore::new(session_id)),
    );
    agent.begin_trace_run("trace code mode", None);
    let mut trace_rx = crate::trace::shared_bus().subscribe();

    let outcome = agent
        .dispatch_tool_call(
            &crate::context::FunctionCall {
                name: "exec".to_string(),
                args: json!({
                    "code": r#"
const response = await tools.echo_tool({ value: "hello" });
text(response.value);
"#
                }),
                id: Some("call_exec_trace".to_string()),
            },
            &tools,
            5,
            agent.trace_context_with_parent(agent.turn_span_id(), Some(1)),
        )
        .await;
    agent.finish_active_trace("run_finished", TraceStatus::Ok, None);

    assert!(!outcome.is_error, "{}", outcome.result);

    let records = collect_trace_records_for_session(&mut trace_rx, session_id, 16).await;
    let exec_started = records
        .iter()
        .find(|record| record.name == "code_mode_exec_started")
        .expect("exec start trace");
    assert_eq!(
        exec_started.attrs["outer_tool_call_id"],
        json!("call_exec_trace")
    );
    assert_eq!(exec_started.attrs["provider"], json!("test-provider"));
    assert_eq!(exec_started.attrs["model"], json!("test-model"));
    assert!(exec_started.attrs["source_length"].as_u64().unwrap() > 0);

    let exec_finished = records
        .iter()
        .find(|record| record.name == "code_mode_exec_finished")
        .expect("exec finish trace");
    assert_eq!(
        exec_finished.attrs["termination_reason"],
        json!("completed")
    );
    assert_eq!(exec_finished.attrs["nested_tool_calls"], json!(1));
    assert!(exec_finished.attrs["output_size_chars"].as_u64().unwrap() > 0);

    let nested_started = records
        .iter()
        .find(|record| {
            record.name == "code_mode_nested_tool_started" && record.kind == TraceKind::SpanStart
        })
        .expect("nested tool start trace");
    assert_eq!(
        nested_started.attrs["outer_tool_call_id"],
        json!("call_exec_trace")
    );
    assert!(nested_started.parent_span_id.is_some());

    cleanup_session(session_id);
}
