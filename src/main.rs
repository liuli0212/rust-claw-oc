mod context;
mod core;
mod discord;
mod llm_client;
mod logging;
mod memory;
pub mod rag;
mod session_manager;
mod skills;
mod telegram;
mod tools;

use crate::core::{AgentOutput, RunExit};
use crate::llm_client::GeminiClient;
use crate::logging::LoggingConfig;
use crate::memory::WorkspaceMemory;
use crate::rag::VectorStore;
use crate::session_manager::SessionManager;
use crate::skills::load_skills;
use crate::tools::{
    BashTool, RagInsertTool, RagSearchTool, ReadFileTool, ReadMemoryTool, TaskPlanTool,
    TavilySearchTool, WebFetchTool, WriteFileTool, WriteMemoryTool,
};
use async_trait::async_trait;
use clap::Parser;
use dotenvy::dotenv;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::sync::Arc;

#[derive(Debug, Parser)]
#[command(name = "rusty-claw", about = "Rusty-Claw CLI agent")]
struct CliArgs {
    /// Log level (e.g. trace, debug, info, warn, error)
    #[arg(long)]
    log_level: Option<String>,
    /// Log directory for file logging
    #[arg(long)]
    log_dir: Option<String>,
    /// Log file name for file logging (daily rotation)
    #[arg(long)]
    log_file: Option<String>,
    /// Disable file logging (stdout only)
    #[arg(long)]
    no_file_log: bool,
    /// Force enable performance report output
    #[arg(long)]
    timing_report: bool,
    /// Disable performance report output
    #[arg(long, conflicts_with = "timing_report")]
    no_timing_report: bool,
    /// Enable prompt report output
    #[arg(long)]
    prompt_report: bool,
}

struct CliOutput;

#[async_trait]
impl AgentOutput for CliOutput {
    async fn on_text(&self, text: &str) {
        print!("{}", text);
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }

    async fn on_tool_start(&self, name: &str, args: &str) {
        println!("\n> [Tool Call]: {} (args: {})", name, args);
    }

    async fn on_tool_end(&self, result: &str) {
        println!("> [Tool Result]: {}", result);
    }

    async fn on_error(&self, error: &str) {
        println!("{}", error);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenv();
    let args = CliArgs::parse();

    if let Some(level) = &args.log_level {
        std::env::set_var("RUST_LOG", level);
    }
    if args.timing_report {
        std::env::set_var("CLAW_TIMING_REPORT", "1");
    } else if args.no_timing_report {
        std::env::set_var("CLAW_TIMING_REPORT", "0");
    }
    if args.prompt_report {
        std::env::set_var("CLAW_PROMPT_REPORT", "1");
    }

    let log_config = LoggingConfig {
        log_level: args.log_level.clone(),
        file_log: if args.no_file_log { Some(false) } else { None },
        log_dir: args.log_dir.clone(),
        log_file: args.log_file.clone(),
    };
    let _log_guard = match logging::init_logging(log_config) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("WARNING: failed to initialize logging: {}", e);
            None
        }
    };

    let api_key = std::env::var("GEMINI_API_KEY").unwrap_or_else(|_| "DUMMY_KEY".to_string());
    if api_key == "DUMMY_KEY" {
        tracing::warn!("GEMINI_API_KEY not set. LLM calls will fail.");
    }

    let llm = Arc::new(GeminiClient::new(api_key));

    let current_dir = std::env::current_dir()?;
    let current_dir_str = current_dir.to_str().unwrap_or(".");
    let workspace = Arc::new(WorkspaceMemory::new(current_dir_str));

    let rag_store = match VectorStore::new() {
        Ok(store) => Some(Arc::new(store)),
        Err(e) => {
            tracing::warn!("Failed to initialize VectorStore: {}", e);
            None
        }
    };

    let mut tools: Vec<Arc<dyn tools::Tool>> = Vec::new();
    tools.push(Arc::new(BashTool::new()));
    tools.push(Arc::new(WriteFileTool));
    tools.push(Arc::new(ReadFileTool));
    tools.push(Arc::new(WebFetchTool::new()));
    tools.push(Arc::new(ReadMemoryTool::new(workspace.clone())));
    tools.push(Arc::new(WriteMemoryTool::new(workspace.clone())));
    tools.push(Arc::new(TaskPlanTool::new(
        current_dir.join(".rusty_claw_task_plan.json"),
    )));

    if let Ok(tavily_api_key) = std::env::var("TAVILY_API_KEY") {
        if !tavily_api_key.trim().is_empty() {
            tools.push(Arc::new(TavilySearchTool::new(tavily_api_key)));
        }
    }

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

    let session_manager = Arc::new(SessionManager::new(llm.clone(), tools.clone()));

    // Start Telegram Bot
    if let Ok(token) = std::env::var("TELEGRAM_BOT_TOKEN") {
        tracing::info!("Starting Telegram bot task");
        let sm = session_manager.clone();
        tokio::spawn(async move {
            telegram::run_telegram_bot(token, sm).await;
        });
    }

    // Start Discord Bot
    if let Ok(token) = std::env::var("DISCORD_BOT_TOKEN") {
        tracing::info!("Starting Discord bot task");
        let sm = session_manager.clone();
        tokio::spawn(async move {
            discord::run_discord_bot(token, sm).await;
        });
    }

    let output = Arc::new(CliOutput);
    // let mut agent = AgentLoop::new(llm, tools, context, output);

    let mut rl = DefaultEditor::new()?;
    println!("Welcome to Rusty-Claw! (type 'exit' to quit)");
    if loaded_count > 0 {
        println!(
            "Loaded {} dynamic skills from 'skills/' directory.",
            loaded_count
        );
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

                let agent = session_manager
                    .get_or_create_session("cli", output.clone())
                    .await;
                let mut agent_guard = agent.lock().await;

                match agent_guard.step(line.to_string()).await {
                    Ok(exit) => match exit {
                        RunExit::CompletedWithReply => {}
                        RunExit::CompletedSilent { cause } => {
                            println!("\n[Run Exit] completed_silent ({})", cause);
                        }
                        RunExit::RecoverableFailed { reason, attempts } => {
                            println!(
                                "\n[Run Exit] recoverable_failed (reason={}, attempts={})",
                                reason, attempts
                            );
                        }
                        RunExit::HardStop { reason } => {
                            println!("\n[Run Exit] hard_stop ({})", reason);
                        }
                    },
                    Err(e) => eprintln!("Agent error: {}", e),
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
