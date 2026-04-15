mod support;

use rusty_claw::context::{AgentContext, FunctionCall};
use rusty_claw::core::{AgentLoop, RunExit};
use rusty_claw::task_state::TaskStateStore;
use rusty_claw::telemetry::TelemetryExporter;
use rusty_claw::tools::Tool;
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

#[tokio::test]
async fn test_code_mode_full_flow_exec_flush_wait_complete() {
    let _workspace = TempWorkspace::new();
    let session_id = format!("test_code_mode_full_{}", uuid::Uuid::new_v4().simple());

    // 1. Setup tools - we need ExecTool and a mock tool for JS to call
    let exec_tool = Arc::new(rusty_claw::tools::ExecTool);
    let wait_tool = Arc::new(rusty_claw::tools::WaitTool);
    let echo_tool = Arc::new(MockTool::new("echo_tool", Ok("echo_result".to_string())));
    let finish_tool = Arc::new(MockTool::new("finish_task", Ok("finished".to_string())));

    let tools: Vec<Arc<dyn Tool>> =
        vec![exec_tool, wait_tool, echo_tool.clone(), finish_tool.clone()];

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
            events: vec![
                ScenarioEvent::Text("Done, finishing task.".to_string()),
                ScenarioEvent::ToolCall(
                    FunctionCall {
                        name: "finish_task".to_string(),
                        args: serde_json::json!({ "summary": "Code mode finished successfully" }),
                        id: Some("call_finish_1".to_string()),
                    },
                    Some("call_finish_1".to_string()),
                ),
            ],
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

    // --- STEP 1: EXECUTE ---
    let result1 = agent.step("Run code mode".to_string()).await.unwrap();
    assert!(
        matches!(result1, RunExit::YieldedToUser),
        "Expected YieldedToUser, got {:?}",
        result1
    );

    {
        let texts = output.texts.lock().await;
        assert!(texts.iter().any(|t| t.contains("Result: echo_result")));
    }

    // --- STEP 2: FINISH TASK ---
    let result2 = agent.step("Wait for it".to_string()).await.unwrap();
    assert!(
        matches!(result2, RunExit::Finished(_)),
        "Expected Finished, got {:?}",
        result2
    );

    {
        let tool_ends = output.tool_ends.lock().await;
        let errors = output.errors.lock().await;
        println!("Captured tool_ends: {:?}", *tool_ends);
        println!("Captured errors: {:?}", *errors);
        assert!(tool_ends.iter().any(|t| t.contains("Resumed")));
    }

    // Verify tool calls
    let echo_calls = echo_tool.calls.lock().await;
    assert_eq!(echo_calls.len(), 1);

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
        setTimeout(() => {
            text("Done");
        }, 30000);
    "#;

    let llm = Arc::new(ScenarioLlm::new(vec![
        ScenarioTurn {
            events: vec![ScenarioEvent::ToolCall(
                FunctionCall {
                    name: "exec".to_string(),
                    args: serde_json::json!({ "code": code }),
                    id: Some("c1".to_string()),
                },
                Some("c1".to_string()),
            )],
        },
        ScenarioTurn {
            events: scripted_wait_turn("c2", "cell-0", 100).events,
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

    // Turn 1: exec. It will start the timer and flush (timer wait).
    let result1 = agent.step("Start".to_string()).await.unwrap();
    assert!(matches!(result1, RunExit::YieldedToUser));

    // Turn 2: wait with 100ms timeout. The timer is far in the future, so this
    // should keep the cell yielded instead of completing.
    let result2 = agent.step("Wait".to_string()).await.unwrap();

    // It should still be YieldedToUser because it hasn't finished.
    assert!(
        matches!(result2, RunExit::YieldedToUser),
        "Expected YieldedToUser, got: {:?}",
        result2
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

    // Register a 500ms timer.
    let code = r#"
        setTimeout(() => {
            text("Timer Fired");
        }, 500);
        text("Running");
    "#;

    let llm = Arc::new(ScenarioLlm::new(vec![
        ScenarioTurn {
            events: vec![ScenarioEvent::ToolCall(
                FunctionCall {
                    name: "exec".to_string(),
                    args: serde_json::json!({ "code": code }),
                    id: Some("c1".to_string()),
                },
                Some("c1".to_string()),
            )],
        },
        scripted_wait_turn("c2", "cell-0", 100),
        scripted_wait_turn("c3", "cell-0", 100),
        scripted_wait_turn("c4", "cell-0", 100),
        scripted_wait_turn("c5", "cell-0", 100),
        scripted_wait_turn("c6", "cell-0", 100),
        scripted_wait_turn("c7", "cell-0", 100),
        scripted_wait_turn("c8", "cell-0", 100),
        scripted_wait_turn("c9", "cell-0", 100),
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

    // Turn 1: exec. It should yield because the timer is still pending.
    let result1 = agent.step("Start".to_string()).await.unwrap();
    assert!(matches!(result1, RunExit::YieldedToUser));
    {
        let tool_ends = output.tool_ends.lock().await;
        assert!(
            tool_ends.iter().any(|t| t.contains("Running")),
            "Expected 'Running' in tool_ends after exec, got: {:?}",
            *tool_ends
        );
    }

    // Poll with short waits until the timer callback is observed. This avoids
    // depending on a single fixed sleep boundary.
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(3);
    loop {
        let exit = agent.step("Wait".to_string()).await.unwrap();
        let saw_timer = {
            let tool_ends = output.tool_ends.lock().await;
            tool_ends.iter().any(|t| t.contains("Timer Fired"))
        };
        if saw_timer {
            break;
        }

        assert!(
            matches!(exit, RunExit::YieldedToUser),
            "Expected timer polling to keep yielding until completion, got: {:?}",
            exit
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "Timed out waiting for timer completion"
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

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
    let finish_tool = Arc::new(MockTool::new("finish_task", Ok("done".to_string())));
    let tools: Vec<Arc<dyn Tool>> = vec![exec_tool, blocking_tool, finish_tool];

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
            events: vec![ScenarioEvent::ToolCall(
                FunctionCall {
                    name: "finish_task".to_string(),
                    args: serde_json::json!({ "summary": "Recovered after cancel" }),
                    id: Some("finish_recovery".to_string()),
                },
                Some("finish_recovery".to_string()),
            )],
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
