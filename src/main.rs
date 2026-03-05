pub mod browser;

mod context;
mod config;
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
mod utils;

use crate::core::{AgentOutput, RunExit};
use crate::memory::WorkspaceMemory;
use crate::rag::VectorStore;
use crate::session_manager::SessionManager;
use crate::skills::load_skills;
use crate::tools::{
    BashTool, RagInsertTool, RagSearchTool, ReadFileTool, ReadMemoryTool,
    TaskPlanTool, TavilySearchTool, WebFetchTool, WriteFileTool, WriteMemoryTool, FinishTaskTool,
    PatchFileTool, SendFileTool,
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
    fn truncate_to_one_line(input: &str) -> String {
        let first_line = input.lines().next().unwrap_or("");
        if first_line.len() > 60 {
            format!("{}...", &first_line.chars().take(57).collect::<String>())
        } else {
            first_line.to_string()
        }
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
        let text = text.replace("<final>", "").replace("</final>", "");
        let text = text.as_str();
        if text.is_empty() {
            return;
        }
        if Self::is_prefixed_status(text) {
            print!("{}", Self::style_status(text));
        } else {
            print!("\x1b[1;97m{}\x1b[0m", text);
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }

    async fn on_thinking(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        print!("\x1b[38;5;244m{}\x1b[0m", text);
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }

    async fn on_tool_start(&self, name: &str, args: &str) {
        let args_val: serde_json::Value = serde_json::from_str(args).unwrap_or(serde_json::json!({}));
        let summary = match name {
            "read_file" | "write_file" | "patch_file" => {
                args_val.get("path").and_then(|v| v.as_str()).unwrap_or("unknown").to_string()
            }
            "execute_bash" => {
                let cmd = args_val.get("command").and_then(|v| v.as_str()).unwrap_or("");
                Self::truncate_to_one_line(cmd)
            }
            "web_fetch" | "browser" => {
                args_val.get("url").or_else(|| args_val.get("target_url")).and_then(|v| v.as_str()).unwrap_or("").to_string()
            }
            _ => Self::truncate_to_one_line(args),
        };

        if summary.is_empty() {
            println!("\x1b[33m> [Tool]: {}\x1b[0m", name);
        } else {
            println!("\x1b[33m> [Tool]: {} ({})\x1b[0m", name, summary);
        }
    }

    async fn on_tool_end(&self, result: &str) {
        let display_result = if result.len() > 100 {
            format!("{}... (total {} chars)", &result.chars().take(80).collect::<String>(), result.len())
        } else {
            result.replace('\n', " ").to_string()
        };
        println!("\x1b[32m> [Result]: {}\x1b[0m", display_result);
    }

    async fn on_error(&self, error: &str) {
        tracing::error!("{}", error);
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
        config.default_provider.clone().unwrap_or(args.provider.clone())
    };

    let llm_init_result = llm_client::create_llm_client(&provider_name, args.model.clone(), &config);
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
    tools.push(Arc::new(TaskPlanTool::new(current_dir.join(".rusty_claw_task_plan.json"))));

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
        tokio::spawn(async move { telegram::run_telegram_bot(token, sm).await; });
    }

    if let Ok(token) = std::env::var("DISCORD_BOT_TOKEN") {
        let sm = session_manager.clone();
        tokio::spawn(async move { discord::run_discord_bot(token, sm).await; });
    }

    let output = Arc::new(CliOutput);
    if let Err(e) = session_manager.get_or_create_session("cli", output.clone()).await {
        eprintln!("\x1b[33m[Warning] Failed to pre-initialize CLI session: {}\x1b[0m", e);
    }

    let mut rl = DefaultEditor::new()?;
    println!("Welcome to Rusty-Claw! (type '/exit' to quit)");
    if std::path::Path::new(".rusty_claw_task_plan.json").exists() {
        println!("\x1b[33m[System] Detected an existing task plan. If you no longer need it, use /cancel_task to clear it.\x1b[0m");
    }

    let sm_clone = session_manager.clone();
    tokio::spawn(async move {
        let mut sigs = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap();
        while let Some(_) = sigs.recv().await { sm_clone.cancel_session("cli").await; }
    });

    let mut ctrl_c_count = 0;
    loop {
        let readline = rl.readline(">> ");
        match readline {
            Ok(line) => {
                ctrl_c_count = 0;
                let line = line.trim();
                if line == "/exit" {
                    break;
                }
                if line == "/new" {
                    session_manager.reset_session("cli").await;
                    let _ = std::fs::remove_file(".rusty_claw_task_plan.json");
                    println!("\x1b[32m[System] Session cleared. Starting fresh.\x1b[0m");
                    continue;
                }
                if line == "/cancel" || line == "/cancel_task" {
                    session_manager.cancel_session("cli").await;
                    let _ = std::fs::remove_file(".rusty_claw_task_plan.json");
                    println!("\x1b[33m[System] Task and plan cancelled.\x1b[0m");
                    continue;
                }
                if line == "/new" {
                    session_manager.reset_session("cli").await;
                    let _ = std::fs::remove_file(".rusty_claw_task_plan.json");
                    println!("\x1b[32m[System] Session cleared. Starting fresh.\x1b[0m");
                    continue;
                }
                if line == "/status" {
                    let agent = session_manager.get_or_create_session("cli", output.clone()).await.unwrap();
                    let agent_guard = agent.lock().await;
                    let (provider, model, tokens, max_tokens) = agent_guard.get_status();
                    let percentage = (tokens as f64 / max_tokens as f64) * 100.0;
                    println!("\x1b[36m[Status]\x1b[0m Provider: {}, Model: {}, Context: {}/{} tokens ({:.1}%)", provider, model, tokens, max_tokens, percentage);
                    continue;
                }
                if line.starts_with("/context") {
                    let agent = session_manager.get_or_create_session("cli", output.clone()).await.unwrap();
                    let mut agent_guard = agent.lock().await;
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    let subcommand = parts.get(1).map(|s| *s).unwrap_or("");

                    match subcommand {
                        "audit" => { println!("{}", agent_guard.get_context_details()); }
                        "diff" => {
                            if let Some(diff) = agent_guard.diff_snapshot() {
                                println!("{}", agent_guard.format_diff(&diff));
                            } else {
                                println!("\x1b[33m[System] No snapshot available for diff. Run a command first.\x1b[0m");
                            }
                        }
                        "inspect" => {
                            let section = parts.get(2).map(|s| *s).unwrap_or("");
                            let arg = parts.get(3).map(|s| *s);
                            if section.is_empty() {
                                println!("\x1b[33m[System] Usage: /context inspect <system|history|memory|plan> [arg]\x1b[0m");
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
                                    println!("\x1b[32m[System] Context dumped to debug_context.json\x1b[0m");
                                } else {
                                    println!("\x1b[31m[System] Failed to write debug_context.json\x1b[0m");
                                }
                            }
                        }
                        "compact" => {
                             match agent_guard.force_compact().await {
                                 Ok(r) => println!("\x1b[32m[System] Context compacted: {}\x1b[0m", r),
                                 Err(e) => println!("\x1b[31m[System] Compaction skipped: {}\x1b[0m", e)
                            }
                        }
                        _ => {
                            let stats = agent_guard.get_detailed_stats();
                            println!("\x1b[36m[Context Usage]\x1b[0m {}/{} tokens ({:.1}%)", stats.total, stats.max, (stats.total as f64 / stats.max as f64) * 100.0);
                            println!("  Identity: {} | Runtime: {} | Custom: {} | Plan: {} | Project: {} | Memory: {} | History: {} | Current: {}", 
                                stats.system_static, stats.system_runtime, stats.system_custom, stats.system_task_plan, stats.system_project, stats.memory, stats.history, stats.current_turn);
                            println!("  Use '/context audit' for deep dive or '/context diff' to see changes.");
                        }
                    }
                    continue;
                }
                if line.starts_with("/model") {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() < 2 {
                        let config = crate::config::AppConfig::load();
                        println!("Usage: /model <provider> [model]\nAvailable: {}", config.providers.keys().cloned().collect::<Vec<_>>().join(", "));
                        continue;
                    }
                    match session_manager.update_session_llm("cli", parts[1], parts.get(2).map(|s| s.to_string())).await {
                        Ok(msg) => println!("\x1b[32m[System] {}\x1b[0m", msg),
                        Err(e) => println!("\x1b[31m[System] Failed: {}\x1b[0m", e),
                    }
                    continue;
                }
                if line.is_empty() { continue; }
                let _ = rl.add_history_entry(line);
                let agent = session_manager.get_or_create_session("cli", output.clone()).await.unwrap();
                let mut agent_guard = agent.lock().await;

                match agent_guard.step(line.to_string()).await {
                    Ok(exit) => match exit {
                        RunExit::YieldedToUser => { println!(); }
                        RunExit::Finished(ref summary) => {
                            println!("\n\x1b[1;32m{}\x1b[0m", summary);
                            println!("\x1b[32m[Run Exit] finished\x1b[0m");
                        }
                        RunExit::StoppedByUser => {
                            println!("\n\x1b[33m[Run Exit] execution_stopped_by_user\x1b[0m");
                            println!("The current operation was manually cancelled.");
                        }
                        RunExit::AgentTurnLimitReached => {
                            println!("\n\x1b[33m[Run Exit] turn_limit_reached\x1b[0m");
                            println!("The agent reached the maximum allowed consecutive actions (to prevent infinite loops).");
                            println!("👉 \x1b[1;36mAction required:\x1b[0m Review the recent actions. If the agent is on the right track, just type \x1b[32m'continue'\x1b[0m to let it proceed.");
                        }
                        RunExit::ContextLimitReached => {
                            println!("\n\x1b[31m[Run Exit] context_limit_reached\x1b[0m");
                            println!("The context window size is exceeding the model's limit.");
                            println!("👉 \x1b[1;36mAction required:\x1b[0m Wait for auto-compaction, or use \x1b[33m/clear\x1b[0m to start a fresh session.");
                        }
                        RunExit::RecoverableFailed(ref msg) => {
                            println!("\n\x1b[33m[Run Exit] recoverable_failure\x1b[0m: {}", msg);
                        }
                        RunExit::CriticallyFailed(ref msg) => {
                            println!("\n\x1b[31m[Run Exit] critical_failure\x1b[0m: {}", msg);
                            println!("The system encountered an unrecoverable error during execution.");
                        }
                    },
                    Err(e) => eprintln!("Agent error: {}", e),
                }

                if std::path::Path::new(".rusty_claw_task_plan.json").exists() {
                    println!("\x1b[33m[System] Current task plan is still active. Use /cancel_task if you want to abort it.\x1b[0m");
                }
            }
            Err(ReadlineError::Interrupted) => {
                ctrl_c_count += 1;
                if ctrl_c_count >= 2 {
                    println!("Exiting.");
                    break;
                } else {
                    println!("\n\x1b[33m[System] Press Ctrl-C again to exit (or type '/exit').\x1b[0m");
                }
            }
            Err(ReadlineError::Eof) => { println!("CTRL-D"); break; }
            Err(err) => { println!("Error: {:?}", err); break; }
        }
    }
    Ok(())
}
