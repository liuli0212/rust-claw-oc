use rusty_claw::app::cli::run_headless_command;
use rusty_claw::session_manager::SessionManager;
use rusty_claw::tools::Tool;
use std::sync::Arc;
use support::capture_output::CaptureOutput;
use support::scenario_llm::{ScenarioEvent, ScenarioLlm, ScenarioTurn};

#[path = "support/mod.rs"]
mod support;

#[tokio::test]
async fn test_cli_headless_command() {
    let tools: Vec<Arc<dyn Tool>> = vec![];

    let llm = Arc::new(ScenarioLlm::new(vec![ScenarioTurn {
        events: vec![ScenarioEvent::Text("I finished the task.".to_string())],
    }]));

    let session_manager = Arc::new(SessionManager::new(Some(llm), tools));
    let output = Arc::new(CaptureOutput::new());

    let result =
        run_headless_command(session_manager, output.clone(), "Do the task".to_string()).await;

    assert!(result.is_ok());

    let texts = output.texts.lock().await;
    assert!(texts
        .iter()
        .any(|t: &String| t.contains("I finished the task.")));
    support::temp_workspace::cleanup_sessions_with_prefix("cli_headless_");
}
