mod context;
mod core;
mod llm_client;
mod memory;
mod tools;
mod skills;
pub mod rag;

use crate::context::AgentContext;
use crate::core::AgentLoop;
use crate::llm_client::GeminiClient;
use crate::memory::WorkspaceMemory;
use crate::rag::VectorStore;
use crate::skills::load_skills;
use crate::tools::{BashTool, ReadMemoryTool, WriteMemoryTool, RagSearchTool, RagInsertTool};
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

    let rag_store = match VectorStore::new() {
        Ok(store) => Some(Arc::new(store)),
        Err(e) => {
            println!("WARNING: Failed to initialize VectorStore: {}", e);
            None
        }
    };

    let mut tools: Vec<Arc<dyn tools::Tool>> = Vec::new();
    tools.push(Arc::new(BashTool::new()));
    tools.push(Arc::new(ReadMemoryTool::new(workspace.clone())));
    tools.push(Arc::new(WriteMemoryTool::new(workspace.clone())));

    if let Some(store) = rag_store {
        tools.push(Arc::new(RagSearchTool::new(store.clone())));
        tools.push(Arc::new(RagInsertTool::new(store.clone())));
    }

    // Load dynamic skills
    let loaded_skills = load_skills("skills");
    let loaded_count = loaded_skills.len();
    for skill in loaded_skills {
        tools.push(Arc::new(skill));
    }

    let context = AgentContext::new();
    let mut agent = AgentLoop::new(llm, tools, context);

    let mut rl = DefaultEditor::new()?;
    println!("Welcome to Rusty-Claw! (type 'exit' to quit)");
    if loaded_count > 0 {
        println!("Loaded {} dynamic skills from 'skills/' directory.", loaded_count);
    }

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
