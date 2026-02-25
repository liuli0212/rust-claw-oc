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
use crate::llm_client::{GeminiClient, LlmClient, OpenAiCompatClient};
use crate::logging::LoggingConfig;
use crate::memory::WorkspaceMemory;
use crate::rag::VectorStore;
use crate::session_manager::SessionManager;
use crate::skills::load_skills;
use crate::tools::{
    BashTool, RagInsertTool, RagSearchTool, ReadFileTool, ReadMemoryTool,
    TaskPlanTool, TavilySearchTool, WebFetchTool, WriteFileTool, WriteMemoryTool, FinishTaskTool,
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
    /// LLM Provider (gemini, aliyun)
    #[arg(long, default_value = "gemini")]
    provider: String,
    /// Model name (e.g. gemini-2.0-flash, qwen-max)
    #[arg(long)]
    model: Option<String>,
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

impl CliOutput {
    fn truncate_to_three_lines(input: &str) -> String {
        let lines: Vec<&str> = input.lines().collect();
        if lines.len() <= 3 {
            return input.to_string();
        }
        format!(
            "{}\n... ({} more lines)",
            lines[..3].join("\n"),
            lines.len() - 3
        )
    }

    fn is_prefixed_status(text: &str) -> bool {
        let t = text.trim_start();
        t.starts_with("[Progress]")
            || t.starts_with("[System]")
            || t.starts_with("[Perf]")
            || t.starts_with("[Prompt Report]")
            || t.starts_with("[Recovery Stats]")
    }

    fn style_status(text: &str) -> String {
        let t = text.trim_start();
        if t.starts_with("[Progress]") {
            format!("\x1b[38;5;245m{}\x1b[0m", text)
        } else if t.starts_with("[System]") {
            format!("\x1b[36m{}\x1b[0m", text)
        } else if t.starts_with("[Perf]") || t.starts_with("[Prompt Report]") {
            format!("\x1b[35m{}\x1b[0m", text)
        } else {
            format!("\x1b[38;5;244m{}\x1b[0m", text)
        }
    }
}

#[async_trait]
impl AgentOutput for CliOutput {
    async fn on_text(&self, text: &str) {
        if Self::is_prefixed_status(text) {
            print!("{}", Self::style_status(text));
        } else {
            // Highlight model-visible reply content so it stands out from progress logs.
            print!("\x1b[1;97m{}\x1b[0m", text);
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }

    async fn on_tool_start(&self, name: &str, args: &str) {
        let display_args = Self::truncate_to_three_lines(args);
        // Suppress noisy output for basic file operations
        if name == "read_file" || name == "write_file" {
            println!(
                "\n\x1b[33m> [Tool Call]: {} ...\x1b[0m",
                name
            );
        } else {
            println!(
                "\n\x1b[33m> [Tool Call]: {} (args: {})\x1b[0m",
                name, display_args
            );
        }
    }

    async fn on_tool_end(&self, result: &str) {
        // Truncate the result to max 3 lines to avoid screen spam
        let display_result = Self::truncate_to_three_lines(result);
        println!("\x1b[32m> [Tool Result]: {}\x1b[0m", display_result);
    }

    async fn on_error(&self, error: &str) {
        println!("\x1b[31m{}\x1b[0m", error);
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

    let llm: Arc<dyn LlmClient> = match args.provider.as_str() {
        "aliyun" => {
            let api_key = std::env::var("DASHSCOPE_API_KEY")
                .expect("DASHSCOPE_API_KEY must be set for aliyun provider");
            let model = args.model.unwrap_or_else(|| "qwen-max".to_string());
            tracing::info!("Using Aliyun provider with model: {}", model);
            Arc::new(OpenAiCompatClient::new(
                api_key,
                "https://coding.dashscope.aliyuncs.com/v1/chat/completions".to_string(),
                model,
            ))
        }
        "gemini" | _ => {
            let api_key = std::env::var("GEMINI_API_KEY")
                .expect("GEMINI_API_KEY must be set for gemini provider");
            let model = args.model.clone();
            tracing::info!(
                "Using Gemini provider with model: {:?}",
                model.as_deref().unwrap_or("default")
            );
            Arc::new(GeminiClient::new(api_key, model))
        }
    };

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
    tools.push(Arc::new(FinishTaskTool));
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

    let mut rl = DefaultEditor::new()?;
    println!("Welcome to Rusty-Claw! (type 'exit' to quit)");
    if loaded_count > 0 {
        println!(
            "Loaded {} dynamic skills from 'skills/' directory.",
            loaded_count
        );
    }

    let sm_clone = session_manager.clone();
    tokio::spawn(async move {
        if let Ok(_) = tokio::signal::ctrl_c().await {
            // First Ctrl+C: Cancel the current agent.
            sm_clone.cancel_session("cli").await;

            // If they press it again, exit?
            // Actually, tokio::signal::ctrl_c() is a one-shot or stream.
            // Let's loop it:
            let mut sigs = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap();
            while let Some(_) = sigs.recv().await {
                sm_clone.cancel_session("cli").await;
            }
        }
    });

    loop {
        let readline = rl.readline(">> ");
        match readline {
            Ok(line) => {
                let line = line.trim();
                if line == "exit" {
                    break;
                }
                if line == "/new" {
                    session_manager.reset_session("cli").await;
                    println!("\x1b[32m[System] Session cleared. Starting fresh.\x1b[0m");
                    continue;
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
                        RunExit::YieldedToUser => {
                            // Message is already printed by core.rs
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
