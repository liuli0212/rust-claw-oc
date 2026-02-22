mod context;
mod core;
mod llm_client;
mod memory;
mod tools;

use crate::context::AgentContext;
use crate::core::AgentLoop;
use crate::llm_client::GeminiClient;
use crate::memory::WorkspaceMemory;
use crate::tools::{BashTool, ReadMemoryTool, WriteMemoryTool};
use dotenvy::dotenv;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenv();

    let api_key = std::env::var("GEMINI_API_KEY").unwrap_or_else(|_| "DUMMY_KEY".to_string());
    if api_key == "DUMMY_KEY" {
        println!("WARNING: GEMINI_API_KEY not set. LLM calls will fail.");
    }

    let llm = Arc::new(GeminiClient::new(api_key));

    let workspace = Arc::new(WorkspaceMemory::new("."));

    let mut tools: Vec<Arc<dyn tools::Tool>> = Vec::new();
    tools.push(Arc::new(BashTool::new()));
    tools.push(Arc::new(ReadMemoryTool::new(workspace.clone())));
    tools.push(Arc::new(WriteMemoryTool::new(workspace.clone())));

    let context = AgentContext::new();
    let mut agent = AgentLoop::new(llm, tools, context);

    let mut rl = DefaultEditor::new()?;
    println!("Welcome to Rusty-Claw! (type 'exit' to quit)");

    loop {
        let readline = rl.readline(">> ");
        match readline {
            Ok(line) => {
                let line = line.trim();
                if line == "exit" {
                    break;
                }
                if line.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(line);

                if let Err(e) = agent.step(line.to_string()).await {
                    eprintln!("Agent error: {}", e);
                }
            }
            Err(ReadlineError::Interrupted) => {
                println!("CTRL-C");
                break;
            }
            Err(ReadlineError::Eof) => {
                println!("CTRL-D");
                break;
            }
            Err(err) => {
                println!("Error: {:?}", err);
                break;
            }
        }
    }

    Ok(())
}
