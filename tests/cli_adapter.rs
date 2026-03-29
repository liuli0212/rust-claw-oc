use rusty_claw::app::cli::run_headless_command;
use rusty_claw::session_manager::SessionManager;
use rusty_claw::tools::Tool;
use std::sync::Arc;
use support::capture_output::CaptureOutput;
use support::scenario_llm::{ScenarioEvent, ScenarioLlm, ScenarioTurn};
use support::test_tools::MockTool;
use rusty_claw::context::FunctionCall;

#[path = "support/mod.rs"]
mod support;

#[tokio::test]
async fn test_cli_headless_command() {
    let _session_id = format!("cli_test_session_{}", uuid::Uuid::new_v4().simple());

    let finish_tool = Arc::new(MockTool::new("finish_task", Ok("finished".to_string())));
    let tools: Vec<Arc<dyn Tool>> = vec![finish_tool.clone()];

    let llm = Arc::new(ScenarioLlm::new(vec![
        ScenarioTurn {
            events: vec![
                ScenarioEvent::Text("I will finish the task.".to_string()),
                ScenarioEvent::ToolCall(
                    FunctionCall {
                        name: "finish_task".to_string(),
                        args: serde_json::json!({"summary": "done"}),
                        id: Some("call_1".to_string()),
                    },
                    Some("call_1".to_string()),
                ),
            ],
        },
    ]));

    let session_manager = Arc::new(SessionManager::new(Some(llm), tools));
    let output = Arc::new(CaptureOutput::new());

    let result = run_headless_command(session_manager, output.clone(), "Do the task".to_string()).await;

    assert!(result.is_ok());

    let texts = output.texts.lock().await;
    assert!(texts.iter().any(|t: &String| t.contains("I will finish the task.")));
    support::temp_workspace::cleanup_session(&_session_id);
}
