pub mod browser;
pub mod event_log;
pub mod schema;
pub mod task_state;
pub mod artifact_store;
pub mod evidence;
pub mod context_assembler;
pub mod telemetry;

<<<<<<< HEAD
=======
pub mod artifact_store;
mod context;
pub mod context_assembler;
>>>>>>> b337582 (feat: Context Management Integration)
mod config;
mod context;
mod core;
mod discord;
pub mod event_log;
pub mod evidence;
mod llm_client;
mod logging;
mod memory;
pub mod rag;
pub mod schema;
mod session_manager;
mod skills;
pub mod task_state;
mod telegram;
pub mod telemetry;
mod tools;
mod ui;
mod utils;

use crate::core::{AgentOutput, RunExit};
use crate::memory::WorkspaceMemory;
use crate::rag::VectorStore;
use crate::session_manager::SessionManager;
use crate::tools::{
    BashTool, FinishTaskTool, PatchFileTool, RagInsertTool, RagSearchTool, ReadFileTool,
    ReadMemoryTool, SendFileTool, TaskPlanTool, TavilySearchTool, WebFetchTool, WriteFileTool,
    WriteMemoryTool,
};
use crate::ui::TuiOutput; // Use the new UI output
use async_trait::async_trait;
use clap::Parser;
use console::style; // For colored text
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

    let config = config::AppConfig::load();
    let _log_guard = match logging::init_logging(&config) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("WARNING: failed to initialize logging: {}", e);
            None
        }
    };

    let provider_name = if args.provider != "gemini" {
        args.provider.clone()
    } else {
        config
            .default_provider
            .clone()
            .unwrap_or(args.provider.clone())
    };

    let llm_init_result =
        llm_client::create_llm_client(&provider_name, args.model.clone(), &config);
    let llm_opt = match llm_init_result {
        Ok(client) => Some(client),
        Err(e) => {
            eprintln!("\x1b[33m[Warning] LLM initialization failed: {}. You can still use /model to switch later.\x1b[0m", e);
            None
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
    tools.push(Arc::new(crate::browser::BrowserTool::new()));
    tools.push(Arc::new(WriteFileTool));
    tools.push(Arc::new(ReadFileTool));
    tools.push(Arc::new(PatchFileTool));
    tools.push(Arc::new(SendFileTool));
    tools.push(Arc::new(FinishTaskTool));
    tools.push(Arc::new(WebFetchTool::new()));
    tools.push(Arc::new(ReadMemoryTool::new(workspace.clone())));
    tools.push(Arc::new(WriteMemoryTool::new(workspace.clone())));


    if let Ok(tavily_api_key) = std::env::var("TAVILY_API_KEY") {
        if !tavily_api_key.trim().is_empty() {
            tools.push(Arc::new(TavilySearchTool::new(tavily_api_key)));
        }
    }

    if let Some(store) = rag_store {
        tools.push(Arc::new(RagSearchTool::new(store.clone())));
        tools.push(Arc::new(RagInsertTool::new(store.clone())));
    }

    // Skills are now dynamically loaded per-turn in the AgentLoop,
    // so we don't load them statically here anymore.

    let session_manager = Arc::new(SessionManager::new(llm_opt, tools.clone()));

    if let Ok(token) = std::env::var("TELEGRAM_BOT_TOKEN") {
        let sm = session_manager.clone();
        tokio::spawn(async move {
            telegram::run_telegram_bot(token, sm).await;
        });
    }

    if let Ok(token) = std::env::var("DISCORD_BOT_TOKEN") {
        let sm = session_manager.clone();
        tokio::spawn(async move {
            discord::run_discord_bot(token, sm).await;
        });
    }

    let output = Arc::new(TuiOutput::new());
    if let Err(e) = session_manager.get_or_create_session("cli", output.clone()).await {
        eprintln!("{} Failed to pre-initialize CLI session: {}", style("⚠").yellow(), e);
    }

    let mut rl = DefaultEditor::new()?;
    
    println!();
    println!("  {}", style("Rust Claw OC").bold().magenta());
    println!("  Type {} to exit, {} for help.", style("/exit").bold(), style("/help").bold());
    println!();

    if std::path::Path::new(".rusty_claw_task_plan.json").exists() {
        println!("  {} Detected an existing task plan. Use {} to clear it.", style("ℹ").blue(), style("/cancel_task").bold());
    }

    let sm_clone = session_manager.clone();
    tokio::spawn(async move {
        let mut sigs =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap();
        while let Some(_) = sigs.recv().await {
            sm_clone.cancel_session("cli").await;
        }
    });

    let mut ctrl_c_count = 0;
    loop {
        let prompt = format!("{} ", style("❯").cyan().bold());
        let readline = rl.readline(&prompt);
        match readline {
            Ok(line) => {
                ctrl_c_count = 0;
                let line = line.trim();
                if line == "/exit" {
                    break;
                }
                if line == "/help" {
                    println!();
                    println!("  {}", style("Available Commands:").bold());
                    println!("  {}  - Start a fresh session", style("/new").green());
                    println!("  {} - Cancel current task", style("/cancel").yellow());
                    println!("  {} - Show model usage", style("/status").cyan());
                    println!("  {} - Switch models", style("/model").magenta());
                    println!("  {} - Inspect context", style("/context").blue());
                    println!();
                    continue;
                }
                if line == "/new" {
                    session_manager.reset_session("cli").await;
                    let _ = std::fs::remove_file(".rusty_claw_task_plan.json");
                    println!("  {} Session cleared. Starting fresh.", style("✔").green());
                    continue;
                }
                if line == "/cancel" || line == "/cancel_task" {
                    session_manager.cancel_session("cli").await;
                    let _ = std::fs::remove_file(".rusty_claw_task_plan.json");
                    println!("  {} Task and plan cancelled.", style("✔").yellow());
                    continue;
                }
                if line == "/status" {
                    let agent = session_manager
                        .get_or_create_session("cli", output.clone())
                        .await
                        .unwrap();
                    let agent_guard = agent.lock().await;
                    let (provider, model, tokens, max_tokens) = agent_guard.get_status();
                    let percentage = (tokens as f64 / max_tokens as f64) * 100.0;
                    println!("  {} Provider: {}, Model: {}, Context: {}/{} tokens ({:.1}%)", 
                        style("📊").cyan(), provider, model, tokens, max_tokens, percentage);
                    continue;
                }
                if line.starts_with("/context") {
                    let agent = session_manager
                        .get_or_create_session("cli", output.clone())
                        .await
                        .unwrap();
                    let mut agent_guard = agent.lock().await;
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    let subcommand = parts.get(1).map(|s| *s).unwrap_or("");

                    match subcommand {
                        "audit" => {
                            println!("{}", agent_guard.get_context_details());
                        }
                        "diff" => {
                            if let Some(diff) = agent_guard.diff_snapshot() {
                                println!("{}", agent_guard.format_diff(&diff));
                            } else {
                                println!("  {} No snapshot available for diff. Run a command first.", style("⚠").yellow());
                            }
                        }
                        "inspect" => {
                            let section = parts.get(2).map(|s| *s).unwrap_or("");
                            let arg = parts.get(3).map(|s| *s);
                            if section.is_empty() {
                                println!("  {} Usage: /context inspect <system|history|memory|plan> [arg]", style("ℹ").blue());
                            } else {
                                println!("{}", agent_guard.inspect_context(section, arg));
                            }
                        }
                        "dump" => {
                            let (payload, sys, report) = agent_guard.build_llm_payload();
                            let dump_data = serde_json::json!({
                                "system_prompt": sys,
                                "messages": payload,
                                "report": {
                                    "max_history_tokens": report.max_history_tokens,
                                    "history_tokens_used": report.history_tokens_used,
                                    "history_turns_included": report.history_turns_included,
                                    "current_turn_tokens": report.current_turn_tokens,
                                    "system_prompt_tokens": report.system_prompt_tokens,
                                    "total_prompt_tokens": report.total_prompt_tokens,
                                    "retrieved_memory_snippets": report.retrieved_memory_snippets,
                                    "retrieved_memory_sources": report.retrieved_memory_sources,
                                },
                                "detailed_stats": {
                                    "system_static": report.detailed_stats.system_static,
                                    "system_runtime": report.detailed_stats.system_runtime,
                                    "system_custom": report.detailed_stats.system_custom,
                                    "system_project": report.detailed_stats.system_project,
                                    "system_task_plan": report.detailed_stats.system_task_plan,
                                    "memory": report.detailed_stats.memory,
                                    "history": report.detailed_stats.history,
                                    "current_turn": report.detailed_stats.current_turn,
                                    "total": report.detailed_stats.total,
                                    "max": report.detailed_stats.max,
                                    "truncated_chars": report.detailed_stats.truncated_chars,
                                }
                            });
                            if let Ok(json_str) = serde_json::to_string_pretty(&dump_data) {
                                if let Ok(_) = std::fs::write("debug_context.json", json_str) {
                                    println!("  {} Context dumped to debug_context.json", style("✔").green());
                                } else {
                                    println!("  {} Failed to write debug_context.json", style("✖").red());
                                }
                            }
                        }
<<<<<<< HEAD
                        "compact" => match agent_guard.force_compact().await {
                            Ok(r) => println!("\x1b[32m[System] Context compacted: {}\x1b[0m", r),
                            Err(e) => println!("\x1b[31m[System] Compaction skipped: {}\x1b[0m", e),
                        },
                        _ => {
                            let stats = agent_guard.get_detailed_stats();
                            println!(
                                "\x1b[36m[Context Usage]\x1b[0m {}/{} tokens ({:.1}%)",
                                stats.total,
                                stats.max,
                                (stats.total as f64 / stats.max as f64) * 100.0
                            );
=======
                        "compact" => {
                             match agent_guard.force_compact().await {
                                 Ok(r) => println!("  {} Context compacted: {}", style("✔").green(), r),
                                 Err(e) => println!("  {} Compaction skipped: {}", style("⚠").yellow(), e)
                            }
                        }
                        _ => {
                            let stats = agent_guard.get_detailed_stats();
                            let pct = (stats.total as f64 / stats.max as f64) * 100.0;
                            println!("  {} {}/{} tokens ({:.1}%)", style("Context Usage").bold().cyan(), stats.total, stats.max, pct);
>>>>>>> 399502f (feat: enhance CLI UI with Markdown rendering, spinners, and icons)
                            println!("  Identity: {} | Runtime: {} | Custom: {} | Plan: {} | Project: {} | Memory: {} | History: {} | Current: {}", 
                                stats.system_static, stats.system_runtime, stats.system_custom, stats.system_task_plan, stats.system_project, stats.memory, stats.history, stats.current_turn);
                            println!("  Use {} for deep dive or {} to see changes.", style("/context audit").bold(), style("/context diff").bold());
                        }
                    }
                    continue;
                }
                if line.starts_with("/model") {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() < 2 {
                        let config = crate::config::AppConfig::load();
                        println!(
                            "Usage: /model <provider> [model]\nAvailable: {}",
                            config
                                .providers
                                .keys()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                        continue;
                    }
<<<<<<< HEAD
                    match session_manager
                        .update_session_llm("cli", parts[1], parts.get(2).map(|s| s.to_string()))
                        .await
                    {
                        Ok(msg) => println!("\x1b[32m[System] {}\x1b[0m", msg),
                        Err(e) => println!("\x1b[31m[System] Failed: {}\x1b[0m", e),
=======
                    match session_manager.update_session_llm("cli", parts[1], parts.get(2).map(|s| s.to_string())).await {
                        Ok(msg) => println!("  {} {}", style("✔").green(), msg),
                        Err(e) => println!("  {} Failed: {}", style("✖").red(), e),
>>>>>>> 399502f (feat: enhance CLI UI with Markdown rendering, spinners, and icons)
                    }
                    continue;
                }
                if line.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(line);
                let agent = session_manager
                    .get_or_create_session("cli", output.clone())
                    .await
                    .unwrap();
                let mut agent_guard = agent.lock().await;

                match agent_guard.step(line.to_string()).await {
                    Ok(exit) => match exit {
                        RunExit::YieldedToUser => {
                            println!();
                        }
                        RunExit::Finished(ref summary) => {
                            println!("\n{}", style(summary).green().bold());
                            println!("  {}", style("Task Finished").green());
                        }
                        RunExit::StoppedByUser => {
                            println!("\n  {}", style("Execution Stopped by User").yellow());
                            println!("  The current operation was manually cancelled.");
                        }
                        RunExit::AgentTurnLimitReached => {
                            println!("\n  {}", style("Turn Limit Reached").yellow());
                            println!("  The agent reached the maximum allowed consecutive actions.");
                            println!("  👉 Action required: Review recent actions. If on track, type {} to proceed.", style("continue").green());
                        }
                        RunExit::ContextLimitReached => {
                            println!("\n  {}", style("Context Limit Reached").red());
                            println!("  The context window size is exceeding the model's limit.");
                            println!("  👉 Action required: Wait for compaction or use {} to start fresh.", style("/new").green());
                        }
                        RunExit::RecoverableFailed(ref msg) => {
                            println!("\n  {} Recoverable Failure: {}", style("⚠").yellow(), msg);
                        }
                        RunExit::CriticallyFailed(ref msg) => {
<<<<<<< HEAD
                            println!("\n\x1b[31m[Run Exit] critical_failure\x1b[0m: {}", msg);
                            println!(
                                "The system encountered an unrecoverable error during execution."
                            );
=======
                            println!("\n  {} Critical Failure: {}", style("✖").red(), msg);
                            println!("  The system encountered an unrecoverable error.");
>>>>>>> 399502f (feat: enhance CLI UI with Markdown rendering, spinners, and icons)
                        }
                    },
                    Err(e) => eprintln!("  {} Agent error: {}", style("✖").red(), e),
                }

                if std::path::Path::new(".rusty_claw_task_plan.json").exists() {
                    println!("  {} Task plan active. Use {} to abort.", style("ℹ").blue(), style("/cancel_task").bold());
                }
            }
            Err(ReadlineError::Interrupted) => {
                ctrl_c_count += 1;
                if ctrl_c_count >= 2 {
                    println!("Exiting.");
                    break;
                } else {
<<<<<<< HEAD
                    println!(
                        "\n\x1b[33m[System] Press Ctrl-C again to exit (or type '/exit').\x1b[0m"
                    );
=======
                    println!("\n  {}", style("Press Ctrl-C again to exit.").yellow());
>>>>>>> 399502f (feat: enhance CLI UI with Markdown rendering, spinners, and icons)
                }
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
