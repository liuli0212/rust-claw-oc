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

#[tokio::test]
async fn test_single_turn_tool_call_and_finish() {
    let _workspace = TempWorkspace::new();
    let session_id = format!("test_session_{}", uuid::Uuid::new_v4().simple());

    // Setup mock tools
    let mock_tool = Arc::new(MockTool::new("mock_tool", Ok("mock_result".to_string())));

    let tools: Vec<Arc<dyn Tool>> = vec![mock_tool.clone()];

    // Setup scenario LLM
    let llm = Arc::new(ScenarioLlm::new(vec![
        ScenarioTurn {
            events: vec![
                ScenarioEvent::Text("I will call the mock tool.".to_string()),
                ScenarioEvent::ToolCall(
                    FunctionCall {
                        name: "mock_tool".to_string(),
                        args: serde_json::json!({"arg": "test"}),
                        id: Some("call_1".to_string()),
                    },
                    Some("call_1".to_string()),
                ),
            ],
        },
        ScenarioTurn {
            events: vec![ScenarioEvent::Text("Now I am finished.".to_string())],
        },
    ]));

    let context = AgentContext::new();
    let output = Arc::new(CaptureOutput::new());
    let (telemetry, _handle) = TelemetryExporter::new();
    let task_state_store = Arc::new(TaskStateStore::new(&session_id));

    let mut agent = AgentLoop::new(
        session_id.clone(),
        llm,
        "cli".to_string(),
        tools,
        context,
        output.clone(),
        Arc::new(telemetry),
        task_state_store,
    );

    let result = agent.step("Do the task".to_string()).await.unwrap();
    assert!(matches!(result, RunExit::Finished(_)));

    let texts = output.texts.lock().await;
    assert!(texts
        .iter()
        .any(|t| t.contains("I will call the mock tool.")));
    assert!(texts.iter().any(|t| t.contains("Now I am finished.")));
    assert_eq!(
        texts
            .iter()
            .filter(|t| t.as_str() == "Now I am finished.")
            .count(),
        1
    );
    let task_finishes = output.task_finishes.lock().await;
    assert_eq!(task_finishes.as_slice(), ["Now I am finished."]);

    let calls = mock_tool.calls.lock().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].get("arg").unwrap().as_str().unwrap(), "test");
    support::temp_workspace::cleanup_session(&session_id);
}

#[tokio::test]
async fn test_read_then_write_file() {
    let workspace = TempWorkspace::new();
    let session_id = format!("test_session_{}", uuid::Uuid::new_v4().simple());

    // Setup real tools
    let read_tool = Arc::new(rusty_claw::tools::files::ReadFileTool);
    let write_tool = Arc::new(rusty_claw::tools::files::WriteFileTool);

    let tools: Vec<Arc<dyn Tool>> = vec![read_tool, write_tool];

    // Pre-create input file
    let input_path = workspace.path().join("input.txt");
    std::fs::write(&input_path, "Hello from input").unwrap();
    let output_path = workspace.path().join("output.txt");

    // Setup scenario LLM
    let llm = Arc::new(ScenarioLlm::new(vec![
        ScenarioTurn {
            events: vec![
                ScenarioEvent::Text("I will read the file.".to_string()),
                ScenarioEvent::ToolCall(
                    FunctionCall {
                        name: "read_file".to_string(),
                        args: serde_json::json!({"path": input_path.to_str().unwrap(), "thought": "read it"}),
                        id: Some("call_1".to_string()),
                    },
                    Some("call_1".to_string()),
                ),
            ],
        },
        ScenarioTurn {
            events: vec![
                ScenarioEvent::Text("I will write the file.".to_string()),
                ScenarioEvent::ToolCall(
                    FunctionCall {
                        name: "write_file".to_string(),
                        args: serde_json::json!({"path": output_path.to_str().unwrap(), "content": "Hello from input - modified", "thought": "write it"}),
                        id: Some("call_2".to_string()),
                    },
                    Some("call_2".to_string()),
                ),
            ],
        },
        ScenarioTurn {
            events: vec![ScenarioEvent::Text("Done writing the file.".to_string())],
        },
    ]));

    let context = AgentContext::new();
    let output = Arc::new(CaptureOutput::new());
    let (telemetry, _handle) = TelemetryExporter::new();
    let task_state_store = Arc::new(TaskStateStore::new(&session_id));

    let mut agent = AgentLoop::new(
        session_id.clone(),
        llm,
        "cli".to_string(),
        tools,
        context,
        output.clone(),
        Arc::new(telemetry),
        task_state_store,
    );

    let result = agent
        .step("Read input.txt and write to output.txt".to_string())
        .await
        .unwrap();
    assert!(matches!(result, RunExit::Finished(_)));

    // Verify output file
    let output_content = std::fs::read_to_string(&output_path).unwrap();
    assert_eq!(output_content, "Hello from input - modified");
    support::temp_workspace::cleanup_session(&session_id);
}

#[tokio::test]
async fn test_tool_failure_and_recovery() {
    let _workspace = TempWorkspace::new();
    let session_id = format!("test_session_{}", uuid::Uuid::new_v4().simple());

    // Setup mock tools
    let flaky_tool = Arc::new(MockTool::with_results(
        "flaky_tool",
        vec![
            Err("Temporary network error".to_string()),
            Ok("Success on second try".to_string()),
        ],
    ));

    let tools: Vec<Arc<dyn Tool>> = vec![flaky_tool.clone()];

    // Setup scenario LLM
    let llm = Arc::new(ScenarioLlm::new(vec![
        ScenarioTurn {
            events: vec![
                ScenarioEvent::Text("I will call the flaky tool.".to_string()),
                ScenarioEvent::ToolCall(
                    FunctionCall {
                        name: "flaky_tool".to_string(),
                        args: serde_json::json!({"arg": "try_1"}),
                        id: Some("call_1".to_string()),
                    },
                    Some("call_1".to_string()),
                ),
            ],
        },
        ScenarioTurn {
            events: vec![
                ScenarioEvent::Text("It failed, I will try again.".to_string()),
                ScenarioEvent::ToolCall(
                    FunctionCall {
                        name: "flaky_tool".to_string(),
                        args: serde_json::json!({"arg": "try_2"}),
                        id: Some("call_2".to_string()),
                    },
                    Some("call_2".to_string()),
                ),
            ],
        },
        ScenarioTurn {
            events: vec![ScenarioEvent::Text(
                "It succeeded; the task is complete.".to_string(),
            )],
        },
    ]));

    let context = AgentContext::new();
    let output = Arc::new(CaptureOutput::new());
    let (telemetry, _handle) = TelemetryExporter::new();
    let task_state_store = Arc::new(TaskStateStore::new(&session_id));

    let mut agent = AgentLoop::new(
        session_id.clone(),
        llm,
        "cli".to_string(),
        tools,
        context,
        output.clone(),
        Arc::new(telemetry),
        task_state_store,
    );

    let result = agent.step("Do the flaky task".to_string()).await.unwrap();
    assert!(matches!(result, RunExit::Finished(_)));

    let errors = output.errors.lock().await;
    assert!(errors.iter().any(|e| e.contains("Temporary network error")));

    let calls = flaky_tool.calls.lock().await;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].get("arg").unwrap().as_str().unwrap(), "try_1");
    assert_eq!(calls[1].get("arg").unwrap().as_str().unwrap(), "try_2");
    support::temp_workspace::cleanup_session(&session_id);
}

#[tokio::test]
async fn test_session_recovery() {
    let _workspace = TempWorkspace::new();
    let session_id = format!("test_session_{}", uuid::Uuid::new_v4().simple());

    let tools: Vec<Arc<dyn Tool>> = vec![];

    // Setup scenario LLM for turn 1
    let llm_turn1 = Arc::new(ScenarioLlm::new(vec![ScenarioTurn {
        events: vec![ScenarioEvent::Text("I am doing turn 1.".to_string())],
    }]));

    let output = Arc::new(CaptureOutput::new());

    // Turn 1
    {
        let session_manager =
            rusty_claw::session_manager::SessionManager::new(Some(llm_turn1), tools.clone());
        let agent_mutex = session_manager
            .get_or_create_session(&session_id.clone(), "cli", output.clone())
            .await
            .unwrap();
        let mut agent = agent_mutex.lock().await;
        let result = agent.step("Do turn 1".to_string()).await.unwrap();
        assert!(matches!(result, RunExit::Finished(_)));
    }

    // Setup scenario LLM for turn 2
    let llm_turn2 = Arc::new(ScenarioLlm::new(vec![ScenarioTurn {
        events: vec![ScenarioEvent::Text(
            "I remember turn 1. Now doing turn 2.".to_string(),
        )],
    }]));

    // Turn 2 with a new SessionManager (simulating restart)
    {
        let session_manager =
            rusty_claw::session_manager::SessionManager::new(Some(llm_turn2), tools.clone());
        let agent_mutex = session_manager
            .get_or_create_session(&session_id.clone(), "cli", output.clone())
            .await
            .unwrap();
        let mut agent = agent_mutex.lock().await;

        // Verify context has history
        let (_, _, turns, _, _) = agent.context.get_context_status();
        assert!(turns > 0, "History should be loaded");

        let result = agent.step("Do turn 2".to_string()).await.unwrap();
        assert!(matches!(result, RunExit::Finished(_)));
    }

    let texts = output.texts.lock().await;
    assert!(texts.iter().any(|t| t.contains("I am doing turn 1.")));
    assert!(texts
        .iter()
        .any(|t| t.contains("I remember turn 1. Now doing turn 2.")));
    support::temp_workspace::cleanup_session(&session_id);
}

#[tokio::test]
async fn test_large_output_compression() {
    let _workspace = TempWorkspace::new();
    let session_id = format!("test_session_{}", uuid::Uuid::new_v4().simple());

    // Create a very large string (e.g., 100KB)
    let large_string = "A".repeat(100_000);

    // Setup mock tools
    let large_tool = Arc::new(MockTool::new("large_tool", Ok(large_string)));

    let tools: Vec<Arc<dyn Tool>> = vec![large_tool.clone()];

    // Setup scenario LLM
    let llm = Arc::new(ScenarioLlm::new(vec![
        ScenarioTurn {
            events: vec![
                ScenarioEvent::Text("I will call the large tool.".to_string()),
                ScenarioEvent::ToolCall(
                    FunctionCall {
                        name: "large_tool".to_string(),
                        args: serde_json::json!({"arg": "test"}),
                        id: Some("call_1".to_string()),
                    },
                    Some("call_1".to_string()),
                ),
            ],
        },
        ScenarioTurn {
            events: vec![ScenarioEvent::Text(
                "I got the large output; the task is complete.".to_string(),
            )],
        },
    ]));

    let mut context = AgentContext::new();
    // Force a small max_tokens to trigger compression
    context.max_history_tokens = 1000;

    let output = Arc::new(CaptureOutput::new());
    let (telemetry, _handle) = TelemetryExporter::new();
    let task_state_store = Arc::new(TaskStateStore::new(&session_id));

    let mut agent = AgentLoop::new(
        session_id.clone(),
        llm,
        "cli".to_string(),
        tools,
        context,
        output.clone(),
        Arc::new(telemetry),
        task_state_store,
    );

    // Turn 1: Call large tool and finish with final text.
    let result1 = agent.step("Do the large task".to_string()).await.unwrap();
    assert!(matches!(result1, RunExit::Finished(_)));

    // Verify that the final answer was displayed and the large tool payload was
    // compacted before being retained in history.
    let texts = output.texts.lock().await;
    assert!(texts
        .iter()
        .any(|t| t.contains("I got the large output; the task is complete.")));
    let large_results: Vec<&str> = agent
        .context
        .dialogue_history
        .iter()
        .flat_map(|turn| turn.messages.iter())
        .flat_map(|message| message.parts.iter())
        .filter_map(|part| part.function_response.as_ref())
        .filter(|response| response.name == "large_tool")
        .filter_map(|response| response.response["result"].as_str())
        .collect();
    assert_eq!(large_results.len(), 1);
    assert!(
        large_results[0].contains("stripped") || large_results[0].len() < 50_000,
        "large tool response was not compacted"
    );
    support::temp_workspace::cleanup_session(&session_id);
}

#[tokio::test]
async fn test_cancel_task() {
    let _workspace = TempWorkspace::new();
    let session_id = format!("test_session_{}", uuid::Uuid::new_v4().simple());

    let blocking_tool = Arc::new(BlockingTool::new("blocking_tool"));
    let tools: Vec<Arc<dyn Tool>> = vec![blocking_tool];

    let llm = Arc::new(ScenarioLlm::new(vec![ScenarioTurn {
        events: vec![
            ScenarioEvent::Text("I will call the blocking tool.".to_string()),
            ScenarioEvent::ToolCall(
                FunctionCall {
                    name: "blocking_tool".to_string(),
                    args: serde_json::json!({}),
                    id: Some("call_1".to_string()),
                },
                Some("call_1".to_string()),
            ),
        ],
    }]));

    let context = AgentContext::new();
    let output = Arc::new(CaptureOutput::new());
    let (telemetry, _handle) = TelemetryExporter::new();
    let task_state_store = Arc::new(TaskStateStore::new(&session_id));

    let mut agent = AgentLoop::new(
        session_id.clone(),
        llm,
        "cli".to_string(),
        tools,
        context,
        output.clone(),
        Arc::new(telemetry),
        task_state_store,
    );

    let cancel_token = agent.cancel_token.clone();
    let cancelled_flag = agent.cancelled.clone();

    let output_for_cancel = output.clone();
    tokio::spawn(async move {
        for _ in 0..200 {
            if !output_for_cancel.tool_starts.lock().await.is_empty() {
                cancelled_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                cancel_token.notify_waiters();
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        cancelled_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        cancel_token.notify_waiters();
    });

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        agent.step("Do the blocking task".to_string()),
    )
    .await
    .expect("Test timed out")
    .unwrap();

    assert!(matches!(result, RunExit::StoppedByUser));
    support::temp_workspace::cleanup_session(&session_id);
}
