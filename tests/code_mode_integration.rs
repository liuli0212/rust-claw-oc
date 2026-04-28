mod support;

use rusty_claw::code_mode::description::CodeModeFormat;
use rusty_claw::code_mode::TEXT_COMMAND_EXEC_SENTINEL;
use rusty_claw::context::{AgentContext, FunctionCall};
use rusty_claw::core::{AgentLoop, RunExit};
use rusty_claw::task_state::TaskStateStore;
use rusty_claw::telemetry::TelemetryExporter;
use rusty_claw::tools::protocol::ToolExecutionEnvelope;
use rusty_claw::tools::Tool;
use serial_test::serial;
use std::sync::Arc;
use support::capture_output::CaptureOutput;
use support::scenario_llm::{ScenarioEvent, ScenarioLlm, ScenarioTurn};
use support::temp_workspace::TempWorkspace;
use support::test_tools::{BlockingTool, MockTool};

fn scripted_wait_turn(call_id: &str, cell_id: &str, wait_timeout_ms: u64) -> ScenarioTurn {
    ScenarioTurn {
        events: vec![ScenarioEvent::ToolCall(
            FunctionCall {
                name: "wait".to_string(),
                args: serde_json::json!({
                    "cell_id": cell_id,
                    "wait_timeout_ms": wait_timeout_ms,
                }),
                id: Some(call_id.to_string()),
            },
            Some(call_id.to_string()),
        )],
    }
}

fn scripted_text_exec_turn(source: &str, call_id: &str) -> ScenarioTurn {
    scripted_text_exec_turn_with_args(source, call_id, serde_json::json!({}))
}

fn scripted_text_exec_turn_with_args(
    source: &str,
    call_id: &str,
    extra_args: serde_json::Value,
) -> ScenarioTurn {
    let mut args = extra_args.as_object().cloned().unwrap_or_default();
    args.insert(
        "code".to_string(),
        serde_json::json!(TEXT_COMMAND_EXEC_SENTINEL),
    );

    ScenarioTurn {
        events: vec![
            ScenarioEvent::Text(source.to_string()),
            ScenarioEvent::ToolCall(
                FunctionCall {
                    name: "exec".to_string(),
                    args: serde_json::Value::Object(args),
                    id: Some(call_id.to_string()),
                },
                Some(format!("sig_{call_id}")),
            ),
        ],
    }
}

#[tokio::test]
#[serial]
async fn test_code_mode_text_command_form_executes_without_json_escaping() {
    let _workspace = TempWorkspace::new();
    let session_id = format!("test_code_mode_text_{}", uuid::Uuid::new_v4().simple());

    let exec_tool = Arc::new(rusty_claw::tools::ExecTool);
    let echo_tool = Arc::new(MockTool::new("echo_tool", Ok("echo_result".to_string())));

    let tools: Vec<Arc<dyn Tool>> = vec![exec_tool, echo_tool.clone()];

    let llm = Arc::new(ScenarioLlm::new(vec![
        scripted_text_exec_turn_with_args(
            r#"const res = await tools.echo_tool({ text: "hello \"quoted\"", pattern: /ExecArgs\s+/ });
text(`text-mode: ${res.output}`);
"#,
            "text_exec_1",
            serde_json::json!({
                "auto_flush_ms": 50,
                "cell_timeout_ms": 120000,
            }),
        ),
        ScenarioTurn {
            events: vec![ScenarioEvent::Text(
                "Text command code mode finished".to_string(),
            )],
        },
    ]));

    let output = Arc::new(CaptureOutput::new());
    let mut agent = AgentLoop::new(
        session_id.clone(),
        llm,
        "cli".to_string(),
        tools,
        AgentContext::new(),
        output.clone(),
        Arc::new(TelemetryExporter::new().0),
        Arc::new(TaskStateStore::new(&session_id)),
    );
    agent.set_code_mode_format(CodeModeFormat::TextCommand);

    let result = agent
        .step("Run text command code mode".to_string())
        .await
        .unwrap();
    assert!(
        matches!(result, RunExit::Finished(_)),
        "Expected Finished, got {:?}",
        result
    );

    let texts = output.texts.lock().await.join("");
    assert!(
        texts.contains("text-mode: echo_result"),
        "Expected code mode output, got: {texts}"
    );
    assert!(
        !texts.contains("tools.echo_tool"),
        "Raw text command source should not be shown to the user: {texts}"
    );

    let echo_calls = echo_tool.calls.lock().await;
    assert_eq!(echo_calls.len(), 1);
    drop(echo_calls);

    let stored_exec = agent
        .context
        .dialogue_history
        .iter()
        .flat_map(|turn| turn.messages.iter())
        .flat_map(|message| message.parts.iter())
        .find_map(|part| {
            part.function_call
                .as_ref()
                .filter(|call| call.name == "exec")
        })
        .expect("stored exec call should exist");
    assert_eq!(
        stored_exec.args["code"],
        serde_json::json!(TEXT_COMMAND_EXEC_SENTINEL)
    );
    assert!(
        !stored_exec.args.to_string().contains("echo_tool"),
        "text-mode source must not be written into the signed exec call"
    );

    support::temp_workspace::cleanup_session(&session_id);
}

#[tokio::test]
#[serial]
async fn test_code_mode_text_command_requires_real_exec_sentinel_tool_call() {
    let _workspace = TempWorkspace::new();
    let session_id = format!(
        "test_code_mode_text_requires_sentinel_{}",
        uuid::Uuid::new_v4().simple()
    );

    let exec_tool = Arc::new(rusty_claw::tools::ExecTool);
    let echo_tool = Arc::new(MockTool::new("echo_tool", Ok("echo_result".to_string())));
    let tools: Vec<Arc<dyn Tool>> = vec![exec_tool, echo_tool.clone()];
    let source = r#"const res = await tools.echo_tool({ text: "hello" });
text(`text-mode: ${res.output}`);
"#;

    let llm = Arc::new(ScenarioLlm::new(vec![
        ScenarioTurn {
            events: vec![
                ScenarioEvent::Text(source.to_string()),
                ScenarioEvent::ToolCall(
                    FunctionCall {
                        name: "exec".to_string(),
                        args: serde_json::json!({
                            "code": source,
                        }),
                        id: Some("text_exec_bad".to_string()),
                    },
                    Some("sig_text_exec_bad".to_string()),
                ),
            ],
        },
        scripted_text_exec_turn(source, "text_exec_retry"),
        ScenarioTurn {
            events: vec![ScenarioEvent::Text(
                "Text command retry finished".to_string(),
            )],
        },
    ]));

    let output = Arc::new(CaptureOutput::new());
    let mut agent = AgentLoop::new(
        session_id.clone(),
        llm,
        "cli".to_string(),
        tools,
        AgentContext::new(),
        output.clone(),
        Arc::new(TelemetryExporter::new().0),
        Arc::new(TaskStateStore::new(&session_id)),
    );
    agent.set_code_mode_format(CodeModeFormat::TextCommand);

    let result = agent
        .step("Run text command code mode".to_string())
        .await
        .unwrap();
    assert!(
        matches!(result, RunExit::Finished(_)),
        "Expected Finished, got {:?}",
        result
    );

    let texts = output.texts.lock().await.join("");
    assert!(
        !texts.contains("tools.echo_tool"),
        "Invalid text-mode source should not be shown to the user: {texts}"
    );
    assert!(
        texts.contains("text-mode: echo_result"),
        "Expected retried code mode output, got: {texts}"
    );

    let echo_calls = echo_tool.calls.lock().await;
    assert_eq!(echo_calls.len(), 1);

    let protocol_error_recorded = agent
        .context
        .dialogue_history
        .iter()
        .flat_map(|turn| turn.messages.iter())
        .flat_map(|message| message.parts.iter())
        .filter_map(|part| part.function_response.as_ref())
        .any(|response| {
            response
                .response
                .get("result")
                .and_then(|result| result.as_str())
                .is_some_and(|result| result.contains("Text code mode protocol error"))
        });
    assert!(
        protocol_error_recorded,
        "Expected invalid exec call to receive a protocol error response"
    );

    support::temp_workspace::cleanup_session(&session_id);
}

#[tokio::test]
async fn test_code_mode_text_command_form_can_wait_for_async_cell() {
    let _workspace = TempWorkspace::new();
    let session_id = format!(
        "test_code_mode_text_async_{}",
        uuid::Uuid::new_v4().simple()
    );

    let exec_tool = Arc::new(rusty_claw::tools::ExecTool);
    let wait_tool = Arc::new(rusty_claw::tools::WaitTool);

    let tools: Vec<Arc<dyn Tool>> = vec![exec_tool, wait_tool];

    let llm = Arc::new(ScenarioLlm::new(vec![
        scripted_text_exec_turn_with_args(
            r#"text("Started");
setTimeout(() => {
    text("Async done");
}, 150);
"#,
            "text_exec_async",
            serde_json::json!({
                "auto_flush_ms": 50,
            }),
        ),
        scripted_wait_turn("wait_text_async", "cell-0", 300),
        ScenarioTurn {
            events: vec![ScenarioEvent::Text(
                "Text command async code mode finished".to_string(),
            )],
        },
    ]));

    let output = Arc::new(CaptureOutput::new());
    let mut agent = AgentLoop::new(
        session_id.clone(),
        llm,
        "cli".to_string(),
        tools,
        AgentContext::new(),
        output.clone(),
        Arc::new(TelemetryExporter::new().0),
        Arc::new(TaskStateStore::new(&session_id)),
    );
    agent.set_code_mode_format(CodeModeFormat::TextCommand);

    let result = agent
        .step("Run async text command code mode".to_string())
        .await
        .unwrap();
    assert!(
        matches!(result, RunExit::Finished(_)),
        "Expected Finished, got {:?}",
        result
    );

    let tool_ends = output.tool_ends.lock().await;
    assert!(
        tool_ends.iter().any(|item| item.contains("Started")),
        "Expected initial text-command output, got: {:?}",
        *tool_ends
    );
    assert!(
        tool_ends.iter().any(|item| item.contains("Async done")),
        "Expected wait to observe async completion, got: {:?}",
        *tool_ends
    );

    support::temp_workspace::cleanup_session(&session_id);
}

#[tokio::test]
#[serial]
async fn test_code_mode_full_flow_exec_flush_wait_complete() {
    let _workspace = TempWorkspace::new();
    let session_id = format!("test_code_mode_full_{}", uuid::Uuid::new_v4().simple());

    // 1. Setup tools - we need ExecTool and a mock tool for JS to call
    let exec_tool = Arc::new(rusty_claw::tools::ExecTool);
    let wait_tool = Arc::new(rusty_claw::tools::WaitTool);
    let echo_tool = Arc::new(MockTool::new("echo_tool", Ok("echo_result".to_string())));

    let tools: Vec<Arc<dyn Tool>> = vec![exec_tool, wait_tool, echo_tool.clone()];

    // 2. Setup scenario LLM
    // Turn 1: Model calls exec. Code calls echo_tool then flushes.
    let code = r#"
        const res = await tools.echo_tool({ text: "hello" });
        text("Result: " + res.output);
        flush({ status: "waiting_for_input" });
        text("Resumed");
    "#;

    let llm = Arc::new(ScenarioLlm::new(vec![
        ScenarioTurn {
            events: vec![
                ScenarioEvent::Text("Running complex logic...".to_string()),
                ScenarioEvent::ToolCall(
                    FunctionCall {
                        name: "exec".to_string(),
                        args: serde_json::json!({ "code": code }),
                        id: Some("call_exec_1".to_string()),
                    },
                    Some("call_exec_1".to_string()),
                ),
            ],
        },
        ScenarioTurn {
            events: vec![
                ScenarioEvent::Text("Cell flushed, I will wait.".to_string()),
                ScenarioEvent::ToolCall(
                    FunctionCall {
                        name: "wait".to_string(),
                        args: serde_json::json!({ "cell_id": "cell-0" }),
                        id: Some("call_wait_1".to_string()),
                    },
                    Some("call_wait_1".to_string()),
                ),
            ],
        },
        ScenarioTurn {
            events: vec![ScenarioEvent::Text(
                "Code mode finished successfully".to_string(),
            )],
        },
    ]));

    let context = AgentContext::new();
    let output = Arc::new(CaptureOutput::new());
    let (telemetry, _handle) = TelemetryExporter::new();
    let task_state_store = Arc::new(TaskStateStore::new(&session_id));

    let mut agent = AgentLoop::new(
        session_id.clone(),
        llm.clone(),
        "cli".to_string(),
        tools.clone(),
        context,
        output.clone(),
        Arc::new(telemetry),
        task_state_store,
    );

    // flush no longer pauses for user input; the agent continues through the
    // scripted wait and final text turn in the same step.
    let result = agent.step("Run code mode".to_string()).await.unwrap();
    assert!(
        matches!(result, RunExit::Finished(_)),
        "Expected Finished, got {:?}",
        result
    );

    {
        let texts = output.texts.lock().await;
        assert!(texts.iter().any(|t| t.contains("Result: echo_result")));
    }

    {
        let tool_ends = output.tool_ends.lock().await;
        let errors = output.errors.lock().await;
        println!("Captured tool_ends: {:?}", *tool_ends);
        println!("Captured errors: {:?}", *errors);
        assert!(tool_ends.iter().any(|t| t.contains("waiting_for_input")));
        assert!(tool_ends.iter().any(|t| t.contains("Resumed")));
    }

    // Verify tool calls
    let echo_calls = echo_tool.calls.lock().await;
    assert_eq!(echo_calls.len(), 1);

    support::temp_workspace::cleanup_session(&session_id);
}

#[tokio::test]
#[serial]
async fn test_code_mode_uses_raw_nested_output_but_fences_llm_payload() {
    let _workspace = TempWorkspace::new();
    let session_id = format!(
        "test_code_mode_raw_nested_output_{}",
        uuid::Uuid::new_v4().simple()
    );

    let exec_tool = Arc::new(rusty_claw::tools::ExecTool);
    let bash_tool = Arc::new(MockTool::new_untrusted(
        "execute_bash",
        Ok("36529 总计".to_string()),
        "execute_bash",
    ));
    let tools: Vec<Arc<dyn Tool>> = vec![exec_tool, bash_tool];

    let code = r#"
        const res = await tools.execute_bash({ command: "wc -l" });
        text(`LOC: ${res.output}`);
    "#;

    let llm = Arc::new(ScenarioLlm::new(vec![
        ScenarioTurn {
            events: vec![ScenarioEvent::ToolCall(
                FunctionCall {
                    name: "exec".to_string(),
                    args: serde_json::json!({ "code": code }),
                    id: Some("call_exec_fenced_display".to_string()),
                },
                Some("call_exec_fenced_display".to_string()),
            )],
        },
        ScenarioTurn {
            events: vec![ScenarioEvent::Text("LOC check finished".to_string())],
        },
    ]));

    let output = Arc::new(CaptureOutput::new());
    let mut agent = AgentLoop::new(
        session_id.clone(),
        llm,
        "cli".to_string(),
        tools,
        AgentContext::new(),
        output.clone(),
        Arc::new(TelemetryExporter::new().0),
        Arc::new(TaskStateStore::new(&session_id)),
    );

    let result = agent.step("Count lines".to_string()).await.unwrap();
    assert!(
        matches!(result, RunExit::Finished(_)),
        "Expected Finished, got {:?}",
        result
    );

    let display_text = output.texts.lock().await.join("");
    let display_errors = output.errors.lock().await.join("\n");
    assert!(
        display_text.contains("LOC: 36529 总计"),
        "Expected raw code-mode display output. texts={display_text:?} errors={display_errors:?}"
    );
    assert!(!display_text.contains("UNTRUSTED_CONTENT"));
    assert!(!display_text.contains("[SECURITY]"));

    let exec_envelope = agent
        .context
        .dialogue_history
        .iter()
        .flat_map(|turn| turn.messages.iter())
        .flat_map(|message| message.parts.iter())
        .filter_map(|part| part.function_response.as_ref())
        .find(|response| response.name == "exec")
        .and_then(|response| response.response.get("result"))
        .and_then(|result| result.as_str())
        .and_then(ToolExecutionEnvelope::from_json_str)
        .expect("exec response envelope should be recorded");
    assert!(
        !exec_envelope.result.output.contains("UNTRUSTED_CONTENT"),
        "Stored/program-path code-mode result should remain raw"
    );
    assert!(exec_envelope.result.output.contains("LOC: 36529 总计"));

    let (messages, _system, _report) = agent.context.build_llm_payload(
        &rusty_claw::task_state::TaskStateSnapshot::empty(),
        &rusty_claw::context_assembler::ContextAssembler::new(100_000),
    );
    let llm_payload = serde_json::to_string(&messages).expect("serialize LLM payload");
    assert!(
        llm_payload.contains("UNTRUSTED_CONTENT"),
        "LLM-facing code-mode result should be security-fenced"
    );
    assert!(
        llm_payload.contains("LOC: 36529 总计"),
        "LLM payload should preserve the code-mode output content"
    );

    support::temp_workspace::cleanup_session(&session_id);
}

#[tokio::test]
async fn test_code_mode_flush_without_value_does_not_throw_type_error() {
    let _workspace = TempWorkspace::new();
    let session_id = format!(
        "test_code_mode_flush_without_value_{}",
        uuid::Uuid::new_v4().simple()
    );

    let exec_tool = Arc::new(rusty_claw::tools::ExecTool);
    let wait_tool = Arc::new(rusty_claw::tools::WaitTool);
    let tools: Vec<Arc<dyn Tool>> = vec![exec_tool, wait_tool];
    let code = r#"
        text("before flush");
        flush();
        text("after flush");
    "#;

    let llm = Arc::new(ScenarioLlm::new(vec![
        ScenarioTurn {
            events: vec![ScenarioEvent::ToolCall(
                FunctionCall {
                    name: "exec".to_string(),
                    args: serde_json::json!({ "code": code }),
                    id: Some("call_exec_flush_undefined".to_string()),
                },
                Some("call_exec_flush_undefined".to_string()),
            )],
        },
        scripted_wait_turn("call_wait_flush_undefined", "cell-0", 300),
        ScenarioTurn {
            events: vec![ScenarioEvent::Text(
                "Flush without value finished".to_string(),
            )],
        },
    ]));

    let output = Arc::new(CaptureOutput::new());
    let mut agent = AgentLoop::new(
        session_id.clone(),
        llm,
        "cli".to_string(),
        tools,
        AgentContext::new(),
        output.clone(),
        Arc::new(TelemetryExporter::new().0),
        Arc::new(TaskStateStore::new(&session_id)),
    );

    let result = agent
        .step("Run flush without value".to_string())
        .await
        .unwrap();
    assert!(
        matches!(result, RunExit::Finished(_)),
        "Expected Finished, got {:?}",
        result
    );

    let rendered = output.texts.lock().await.join("\n");
    let errors = output.errors.lock().await.join("\n");
    assert!(rendered.contains("before flush"));
    assert!(rendered.contains("after flush"));
    assert!(
        !errors.contains("undefined"),
        "flush() should not pass JS undefined into Rust host callbacks: {errors}"
    );

    support::temp_workspace::cleanup_session(&session_id);
}

#[tokio::test]
async fn test_code_mode_wait_timeout() {
    let _workspace = TempWorkspace::new();
    let session_id = format!("test_code_mode_timeout_{}", uuid::Uuid::new_v4().simple());

    let exec_tool = Arc::new(rusty_claw::tools::ExecTool);
    let wait_tool = Arc::new(rusty_claw::tools::WaitTool);

    let tools: Vec<Arc<dyn Tool>> = vec![exec_tool, wait_tool];

    // Code that uses a timer. The environment will flush at the end of execution
    // because it sees a pending timer.
    let code = r#"
        text("Started");
        setTimeout(() => {
            text("Done");
        }, 30000);
    "#;

    let llm = Arc::new(ScenarioLlm::new(vec![
        ScenarioTurn {
            events: vec![ScenarioEvent::ToolCall(
                FunctionCall {
                    name: "exec".to_string(),
                    args: serde_json::json!({
                        "code": code,
                        "auto_flush_ms": 50,
                    }),
                    id: Some("c1".to_string()),
                },
                Some("c1".to_string()),
            )],
        },
        ScenarioTurn {
            events: scripted_wait_turn("c2", "cell-0", 100).events,
        },
        ScenarioTurn {
            events: vec![ScenarioEvent::Text(
                "Observed wait timeout snapshot".to_string(),
            )],
        },
    ]));

    let output = Arc::new(CaptureOutput::new());
    let mut agent = AgentLoop::new(
        session_id.clone(),
        llm.clone(),
        "cli".to_string(),
        tools,
        AgentContext::new(),
        output.clone(),
        Arc::new(TelemetryExporter::new().0),
        Arc::new(TaskStateStore::new(&session_id)),
    );

    // The auto-flush progress update is displayed, but it no longer forces a
    // YieldedToUser boundary; the scripted wait and final text turn can continue
    // inside the same step.
    let result = agent.step("Start".to_string()).await.unwrap();

    assert!(
        matches!(result, RunExit::Finished(_)),
        "Expected the scripted final text turn to complete, got: {:?}",
        result
    );

    let tool_ends = output.tool_ends.lock().await;
    assert!(
        tool_ends.iter().any(|item| item.contains("still running")),
        "Expected wait timeout to report a running snapshot, got: {:?}",
        *tool_ends
    );
    assert!(
        !tool_ends.iter().any(|item| item.contains("Done")),
        "Wait timeout should not report timer completion, got: {:?}",
        *tool_ends
    );

    support::temp_workspace::cleanup_session(&session_id);
}

#[tokio::test]
async fn test_code_mode_multi_cell_session() {
    let _workspace = TempWorkspace::new();
    let session_id = format!("test_code_mode_multi_{}", uuid::Uuid::new_v4().simple());

    let exec_tool = Arc::new(rusty_claw::tools::ExecTool);
    let tools: Vec<Arc<dyn Tool>> = vec![exec_tool];

    let llm = Arc::new(ScenarioLlm::new(vec![
        ScenarioTurn {
            events: vec![ScenarioEvent::ToolCall(
                FunctionCall {
                    name: "exec".to_string(),
                    args: serde_json::json!({ "code": "store('foo', { a: 1 }); text('Stored');" }),
                    id: Some("c1".to_string()),
                },
                Some("c1".to_string()),
            )],
        },
        ScenarioTurn {
            events: vec![ScenarioEvent::ToolCall(
                FunctionCall {
                    name: "exec".to_string(),
                    args: serde_json::json!({ "code": "const v = load('foo'); text('Loaded ' + JSON.stringify(v));" }),
                    id: Some("c2".to_string()),
                },
                Some("c2".to_string()),
            )],
        },
    ]));

    let output = Arc::new(CaptureOutput::new());
    let (telemetry, _handle) = TelemetryExporter::new();
    let mut agent = AgentLoop::new(
        session_id.clone(),
        llm.clone(),
        "cli".to_string(),
        tools,
        AgentContext::new(),
        output.clone(),
        Arc::new(telemetry),
        Arc::new(TaskStateStore::new(&session_id)),
    );

    // Cell 1: store
    let _ = agent.step("Cell 1".to_string()).await.unwrap();
    {
        let texts = output.texts.lock().await;
        assert!(texts.iter().any(|t| t.contains("Stored")));
    }

    // Cell 2: load
    let _ = agent.step("Cell 2".to_string()).await.unwrap();
    {
        let texts = output.texts.lock().await;
        assert!(texts.iter().any(|t| t.contains("Loaded {\"a\":1}")));
    }

    support::temp_workspace::cleanup_session(&session_id);
}

#[tokio::test]
async fn test_code_mode_error_propagation() {
    let _workspace = TempWorkspace::new();
    let session_id = format!("test_code_mode_error_{}", uuid::Uuid::new_v4().simple());

    let exec_tool = Arc::new(rusty_claw::tools::ExecTool);
    let tools: Vec<Arc<dyn Tool>> = vec![exec_tool];

    // Code with a runtime error (calling undefined function)
    let code = "nonExistentFunction();";

    let llm = Arc::new(ScenarioLlm::new(vec![ScenarioTurn {
        events: vec![ScenarioEvent::ToolCall(
            FunctionCall {
                name: "exec".to_string(),
                args: serde_json::json!({ "code": code }),
                id: Some("c1".to_string()),
            },
            Some("c1".to_string()),
        )],
    }]));

    let output = Arc::new(CaptureOutput::new());
    let (telemetry, _handle) = TelemetryExporter::new();
    let mut agent = AgentLoop::new(
        session_id.clone(),
        llm.clone(),
        "cli".to_string(),
        tools,
        AgentContext::new(),
        output.clone(),
        Arc::new(telemetry),
        Arc::new(TaskStateStore::new(&session_id)),
    );

    // Turn 1: exec.
    let _ = agent.step("Start".to_string()).await.unwrap();

    // Check output for error — errors are captured in tool_ends (tool result)
    // or errors (on_error). The runtime error shows in the tool output envelope.
    {
        let tool_ends = output.tool_ends.lock().await;
        let errors = output.errors.lock().await;
        let has_error = tool_ends
            .iter()
            .any(|t| t.contains("ReferenceError") || t.contains("not defined"))
            || errors
                .iter()
                .any(|t| t.contains("ReferenceError") || t.contains("not defined"));
        if !has_error {
            panic!(
                "Expected error info in output, but got tool_ends: {:?}, errors: {:?}",
                *tool_ends, *errors
            );
        }
    }

    support::temp_workspace::cleanup_session(&session_id);
}

#[tokio::test]
#[serial]
async fn test_code_mode_nested_tool_error() {
    let _workspace = TempWorkspace::new();
    let session_id = format!("test_code_mode_nested_{}", uuid::Uuid::new_v4().simple());

    let exec_tool = Arc::new(rusty_claw::tools::ExecTool);
    let mock_tool = Arc::new(MockTool::new(
        "fail_tool",
        Err("Tool failed intentionally".to_string()),
    ));
    let tools: Vec<Arc<dyn Tool>> = vec![exec_tool, mock_tool];

    let code = r#"
        try {
            await tools.fail_tool({});
            text("Success");
        } catch (e) {
            text("Caught: " + e);
        }
    "#;

    let llm = Arc::new(ScenarioLlm::new(vec![ScenarioTurn {
        events: vec![ScenarioEvent::ToolCall(
            FunctionCall {
                name: "exec".to_string(),
                args: serde_json::json!({ "code": code }),
                id: Some("c1".to_string()),
            },
            Some("c1".to_string()),
        )],
    }]));

    let output = Arc::new(CaptureOutput::new());
    let (telemetry, _handle) = TelemetryExporter::new();
    let mut agent = AgentLoop::new(
        session_id.clone(),
        llm.clone(),
        "cli".to_string(),
        tools,
        AgentContext::new(),
        output.clone(),
        Arc::new(telemetry),
        Arc::new(TaskStateStore::new(&session_id)),
    );

    // Turn 1: exec. Code runs to completion (try/catch handles the error).
    let _ = agent.step("Start".to_string()).await.unwrap();

    {
        let tool_ends = output.tool_ends.lock().await;
        assert!(
            tool_ends
                .iter()
                .any(|t| t.contains("Tool failed intentionally")),
            "Expected 'Tool failed intentionally' in tool_ends, got: {:?}",
            *tool_ends
        );
        assert!(
            !tool_ends.iter().any(|t| t.contains("\"Success\"")),
            "Should not contain 'Success' in tool_ends"
        );
    }

    support::temp_workspace::cleanup_session(&session_id);
}

#[tokio::test]
async fn test_code_mode_timer_completion() {
    let _workspace = TempWorkspace::new();
    let session_id = format!(
        "test_code_mode_timer_completion_{}",
        uuid::Uuid::new_v4().simple()
    );

    let exec_tool = Arc::new(rusty_claw::tools::ExecTool);
    let wait_tool = Arc::new(rusty_claw::tools::WaitTool);
    let tools: Vec<Arc<dyn Tool>> = vec![exec_tool, wait_tool];

    // Register a short timer and let the host auto-publish progress while it
    // is pending before a subsequent wait observes the completion.
    let code = r#"
        setTimeout(() => {
            text("Timer Fired");
        }, 150);
        text("Running");
    "#;

    let llm = Arc::new(ScenarioLlm::new(vec![
        ScenarioTurn {
            events: vec![ScenarioEvent::ToolCall(
                FunctionCall {
                    name: "exec".to_string(),
                    args: serde_json::json!({
                        "code": code,
                        "auto_flush_ms": 50,
                    }),
                    id: Some("c1".to_string()),
                },
                Some("c1".to_string()),
            )],
        },
        scripted_wait_turn("c2", "cell-0", 300),
        ScenarioTurn {
            events: vec![ScenarioEvent::Text("Observed timer completion".to_string())],
        },
    ]));

    let output = Arc::new(CaptureOutput::new());
    let (telemetry, _handle) = TelemetryExporter::new();
    let mut agent = AgentLoop::new(
        session_id.clone(),
        llm.clone(),
        "cli".to_string(),
        tools,
        AgentContext::new(),
        output.clone(),
        Arc::new(telemetry),
        Arc::new(TaskStateStore::new(&session_id)),
    );

    let exit = agent.step("Start".to_string()).await.unwrap();
    assert!(
        matches!(exit, RunExit::Finished(_)),
        "Expected exec, wait, and final text to complete in one step, got: {:?}",
        exit
    );

    let tool_ends = output.tool_ends.lock().await;
    assert!(
        tool_ends.iter().any(|t| t.contains("Running")),
        "Expected 'Running' in tool_ends after exec, got: {:?}",
        *tool_ends
    );
    assert!(
        tool_ends.iter().any(|t| t.contains("Timer Fired")),
        "Expected timer completion in tool_ends, got: {:?}",
        *tool_ends
    );

    support::temp_workspace::cleanup_session(&session_id);
}

#[tokio::test]
async fn test_code_mode_cancel_clears_active_cell_for_next_exec() {
    let _workspace = TempWorkspace::new();
    let session_id = format!(
        "test_code_mode_cancel_cleanup_{}",
        uuid::Uuid::new_v4().simple()
    );

    let exec_tool = Arc::new(rusty_claw::tools::ExecTool);
    let blocking_tool = Arc::new(BlockingTool::new("blocker"));
    let tools: Vec<Arc<dyn Tool>> = vec![exec_tool, blocking_tool];

    let llm = Arc::new(ScenarioLlm::new(vec![
        ScenarioTurn {
            events: vec![ScenarioEvent::ToolCall(
                FunctionCall {
                    name: "exec".to_string(),
                    args: serde_json::json!({
                        "code": "await tools.blocker({}); text('unreachable');"
                    }),
                    id: Some("cancel_exec".to_string()),
                },
                Some("cancel_exec".to_string()),
            )],
        },
        ScenarioTurn {
            events: vec![ScenarioEvent::ToolCall(
                FunctionCall {
                    name: "exec".to_string(),
                    args: serde_json::json!({ "code": "text('Recovered');" }),
                    id: Some("recovery_exec".to_string()),
                },
                Some("recovery_exec".to_string()),
            )],
        },
        ScenarioTurn {
            events: vec![ScenarioEvent::Text("Recovered after cancel".to_string())],
        },
    ]));

    let output = Arc::new(CaptureOutput::new());
    let (telemetry, _handle) = TelemetryExporter::new();
    let mut agent = AgentLoop::new(
        session_id.clone(),
        llm,
        "cli".to_string(),
        tools,
        AgentContext::new(),
        output.clone(),
        Arc::new(telemetry),
        Arc::new(TaskStateStore::new(&session_id)),
    );

    let cancel_token = agent.cancel_token.clone();
    let cancelled = agent.cancelled.clone();
    let first_step = tokio::spawn(async move {
        let result = agent.step("Run blocking code mode".to_string()).await;
        (agent, result)
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    cancel_token.notify_waiters();

    let (mut agent, first_result) = first_step.await.unwrap();
    assert!(matches!(first_result.unwrap(), RunExit::StoppedByUser));

    let second_result = agent
        .step("Run recovery code mode".to_string())
        .await
        .unwrap();
    assert!(
        matches!(second_result, RunExit::Finished(_)),
        "Expected recovery step to finish, got: {:?}",
        second_result
    );

    let tool_ends = output.tool_ends.lock().await;
    assert!(
        tool_ends.iter().any(|item| item.contains("Recovered")),
        "Expected recovery exec output, got: {:?}",
        *tool_ends
    );
    assert!(
        !tool_ends.iter().any(|item| item.contains("still active")),
        "Active cell should have been cleared after cancel, got: {:?}",
        *tool_ends
    );

    support::temp_workspace::cleanup_session(&session_id);
}
