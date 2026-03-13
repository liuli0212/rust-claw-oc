#[cfg(feature = "acp")]
pub mod acp;
pub mod lsp;
pub mod browser;
pub mod context_assembler;
pub mod event_log;
pub mod evidence;
pub mod rag;
pub mod schema;
pub mod task_state;
pub mod telemetry;

mod config;
mod context;
mod core;
mod discord;
mod llm_client;
mod logging;
mod memory;
mod session_manager;
mod skills;
mod telegram;
mod tools;
mod ui;
mod utils;

use crate::core::{AgentOutput, RunExit};
use crate::memory::WorkspaceMemory;
use crate::rag::VectorStore;
use crate::session_manager::SessionManager;
use crate::tools::{
    BashTool, PatchFileTool, RagInsertTool, RagSearchTool, ReadFileTool, ReadMemoryTool,
    SendFileTool, TavilySearchTool, WebFetchTool, WriteFileTool, WriteMemoryTool,
};
use crate::ui::TuiOutput;
use clap::Parser;
use console::style;
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
    /// Enable prompt caching (if supported by the provider)
    #[arg(long)]
    cache: bool,
    /// Gemini platform (gen, vertex). Defaults to vertex if not specified.
    #[arg(long)]
    gemini_platform: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();
    let args = CliArgs::parse();

    println!();
    println!("  {}", style("Rusty-Claw AGENT-OS v0.1.0").bold().cyan());
    println!("  {}", style("---------------------------").dim());

    let config = config::AppConfig::load();
    let _guards = logging::init_logging(&config);

    let llm_opt = match llm_client::create_llm_client(
        &args.provider,
        args.model.clone(),
        args.gemini_platform.clone(),
        &config,
    ) {
        Ok(llm) => Some(llm),
        Err(e) => {
            tracing::error!("Failed to initialize default LLM: {}", e);
            println!(
                "  {} Failed to initialize default LLM: {}",
                style("⚠️").yellow(),
                e
            );
            println!(
                "  Starting without default LLM. Use {} to configure one.",
                style("/model <provider> [model_name]").bold()
            );
            None
        }
    };

    // [OOM-TEST] Temporarily disabled to isolate memory leak
    let vector_store = Arc::new(VectorStore::new()?);
    let workspace_memory = Arc::new(WorkspaceMemory::new("."));

    let tavily_key = std::env::var("TAVILY_API_KEY").unwrap_or_default();

    let mut tools: Vec<Arc<dyn tools::Tool>> = vec![
        Arc::new(BashTool::new()),
        Arc::new(WriteFileTool),
        Arc::new(ReadFileTool),
        Arc::new(PatchFileTool),
        Arc::new(TavilySearchTool::new(tavily_key)),
        Arc::new(WebFetchTool::new()),
        // [OOM-TEST] Temporarily disabled to isolate memory leak
        Arc::new(RagSearchTool::new(vector_store.clone())),
        Arc::new(RagInsertTool::new(vector_store.clone())),
        Arc::new(ReadMemoryTool::new(workspace_memory.clone())),
        Arc::new(WriteMemoryTool::new(workspace_memory.clone())),
        Arc::new(SendFileTool),
    ];

    // Initialize LSP Client
    let lsp_client = match lsp::LspClient::start(std::env::current_dir()?).await {
        Ok(client) => {
            println!("  {} Rust LSP (rust-analyzer) initialized.", style("✔").green());
            Some(client)
        }
        Err(e) => {
            println!("  {} Failed to start Rust LSP: {}", style("⚠️").yellow(), e);
            None
        }
    };

    if let Some(client) = lsp_client {
        tools.push(Arc::new(tools::LspGotoDefinitionTool { lsp_client: client.clone() }));
        tools.push(Arc::new(tools::LspFindReferencesTool { lsp_client: client.clone() }));
        tools.push(Arc::new(tools::LspHoverTool { lsp_client: client.clone() }));
        tools.push(Arc::new(tools::LspGetDiagnosticsTool { lsp_client: client.clone() }));
        tools.push(Arc::new(tools::LspGetSymbolsTool { lsp_client: client.clone() }));
    }

    let telegram_token = std::env::var("TELEGRAM_BOT_TOKEN").ok();
    if let Some(ref token) = telegram_token {
        tools.push(Arc::new(tools::SendTelegramMessageTool::new(token.clone())));
    }

    let session_manager = Arc::new(SessionManager::new(llm_opt, tools.clone()));

    if let Some(token) = telegram_token {
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

    #[cfg(feature = "acp")]
    if let Ok(port_str) = std::env::var("ACP_PORT") {
        if let Ok(port) = port_str.parse::<u16>() {
            let sm = session_manager.clone();
            tokio::spawn(async move {
                let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
                let acp_server = acp::AcpServer::new(sm);
                if let Err(e) = acp_server.run(addr).await {
                    tracing::error!("ACP server failed: {}", e);
                }
            });
        }
    }

    let mut rl = DefaultEditor::new()?;
    let output = Arc::new(TuiOutput::new());

    println!(
        "  Type {} to exit, {} for help, end line with {} for multi-line.",
        style("/exit").bold(),
        style("/help").bold(),
        style("\\").bold()
    );
    println!();

    let sm_clone = session_manager.clone();
    tokio::spawn(async move {
        let mut sigs =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap();
        while sigs.recv().await.is_some() {
            sm_clone.cancel_session("cli").await;
        }
    });

    let task_store_check_cli = crate::task_state::TaskStateStore::new("cli");
    if task_store_check_cli.has_active_plan() {
        if let Ok(state) = task_store_check_cli.load() {
            println!(
                "  {} Task plan active: {}",
                style("🎯").yellow(),
                style(state.goal.unwrap_or_default()).bold()
            );
            println!(
                "  {} You can say {} to proceed, or {} to abort.",
                style("ℹ").blue(),
                style("\"continue\"").green().bold(),
                style("/cancel_task").red().bold()
            );
        }
    }

    let mut ctrl_c_count = 0;
    let mut current_input = String::new();
    loop {
        let prompt = if current_input.is_empty() {
            format!("{} ", style("❯").cyan().bold())
        } else {
            format!("{} ", style("..").dim())
        };

        let readline = rl.readline(&prompt);
        let line = match readline {
            Ok(line) => line,
            Err(ReadlineError::Interrupted) => {
                ctrl_c_count += 1;
                if ctrl_c_count >= 2 {
                    println!("\n  Exiting...");
                    break;
                }
                println!("\n  {}", style("Press Ctrl-C again to exit.").yellow());
                session_manager.cancel_session("cli").await;
                continue;
            }
            Err(ReadlineError::Eof) => {
                break;
            }
            Err(err) => {
                println!("  {} Error: {:?}", style("❌").red(), err);
                break;
            }
        };

        ctrl_c_count = 0;

        if line.ends_with('\\') {
            current_input.push_str(&line[..line.len() - 1]);
            current_input.push('\n');
            continue;
        }

        current_input.push_str(&line);
        let line = current_input.trim().to_string();

        if line.is_empty() {
            current_input.clear();
            continue;
        }

        // Add to memory history
        rl.add_history_entry(&current_input).ok();
        current_input.clear();

        if line == "/exit" {
            break;
        }
        if line == "/help" {
            println!();
            println!("  {}", style("Available Commands:").bold());
            println!("  {}  - Start a fresh session", style("/new").green());
            println!(
                "  {} - Cancel current API request",
                style("/cancel").yellow()
            );
            println!("  {} - Abort active task plan", style("/cancel_task").red());
            println!("  {} - Show model usage", style("/status").cyan());
            println!("  {} - Switch models", style("/model").magenta());
            println!("  {} - List all sessions", style("/session").white());
            println!("  {} - Inspect context", style("/context").blue());
            println!();
            continue;
        }
        if line == "/new" {
            session_manager.reset_session("cli").await;
            let ts = crate::task_state::TaskStateStore::new("cli");
            let _ = ts.clear();
            println!("  {} Session cleared. Starting fresh.", style("✔").green());
            continue;
        }
        if line == "/cancel" {
            session_manager.cancel_session("cli").await;
            println!("  {} Request cancelled.", style("✔").yellow());
            continue;
        }
        if line == "/cancel_task" {
            session_manager.cancel_session("cli").await;
            let ts = crate::task_state::TaskStateStore::new("cli");
            let _ = ts.clear();
            println!("  {} Task cancelled and plan cleared.", style("✔").yellow());
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
            println!(
                "  {} Provider: {}, Model: {}, Context: {}/{} tokens ({:.1}%)",
                style("📊").cyan(),
                provider,
                model,
                tokens,
                max_tokens,
                percentage
            );
            let ts = crate::task_state::TaskStateStore::new("cli");
            if ts.has_active_plan() {
                if let Ok(state) = ts.load() {
                    println!(
                        "  {} Active Task: {}",
                        style("🎯").yellow(),
                        state.goal.unwrap_or_else(|| "Unknown".to_string())
                    );
                    for (i, step) in state.plan_steps.iter().enumerate() {
                        let icon = match step.status.as_str() {
                            "completed" => "✅",
                            "in_progress" => "⏳",
                            _ => "⬜",
                        };
                        println!("    [{}] {} {}", i, icon, step.step);
                    }
                }
            }
            continue;
        }
        if line == "/session" {
            let sessions = session_manager.list_sessions();
            println!();
            println!("  {}", style("Active/Recent Sessions:").bold());
            if sessions.is_empty() {
                println!("    (No sessions found)");
            } else {
                for (id, updated, turns) in sessions {
                    let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(updated);
                    let datetime: chrono::DateTime<chrono::Local> = chrono::DateTime::from(time);
                    println!(
                        "    {} - {} (Turns: {}, Last Updated: {})",
                        style("•").cyan(),
                        style(id).bold(),
                        turns,
                        datetime.format("%Y-%m-%d %H:%M:%S")
                    );
                }
            }
            println!();
            continue;
        }
        if line.starts_with("/context") {
            let agent = session_manager
                .get_or_create_session("cli", output.clone())
                .await
                .unwrap();
            let mut agent_guard = agent.lock().await;
            let parts: Vec<&str> = line.split_whitespace().collect();
            let subcommand = parts.get(1).copied().unwrap_or("");

            match subcommand {
                "audit" => {
                    println!("{}", agent_guard.get_context_details());
                }
                "diff" => {
                    if let Some(diff) = agent_guard.diff_snapshot() {
                        println!("{}", agent_guard.format_diff(&diff));
                    } else {
                        println!("  {} No changes since last snapshot.", style("ℹ").blue());
                    }
                }
                "inspect" => {
                    let section = parts.get(2).copied().unwrap_or("");
                    let arg = parts.get(3).copied();
                    if section.is_empty() {
                        println!(
                            "  {} Usage: /context inspect <system|history|memory|plan> [arg]",
                            style("ℹ").blue()
                        );
                    } else {
                        println!("{}", agent_guard.inspect_context(section, arg));
                    }
                }
                "dump" => {
                    let (payload, sys, report) = agent_guard.build_llm_payload();
                    let dump_data = serde_json::json!({
                        "system_prompt": sys,
                        "messages": payload,
                        "tools": agent_guard.get_tools_metadata(),
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
                        if std::fs::write("debug_context.json", json_str).is_ok() {
                            println!(
                                "  {} Context dumped to debug_context.json",
                                style("✔").green()
                            );
                        } else {
                            println!("  {} Failed to write debug_context.json", style("✖").red());
                        }
                    }
                }
                "compact" => {
                    println!("  {} Attempting manual compaction...", style("⚙").yellow());
                    match agent_guard.maybe_compact_history(true).await {
                        Ok(_) => println!("  {} Compaction attempt finished.", style("✔").green()),
                        Err(e) => {
                            println!("  {} Compaction failed: {}", style("❌").red(), e)
                        }
                    }
                }
                _ => {
                    let stats = agent_guard.get_detailed_stats();
                    let pct = (stats.total as f64 / stats.max as f64) * 100.0;
                    println!(
                        "  {} {}/{} tokens ({:.1}%)",
                        style("Context Usage").bold().cyan(),
                        stats.total,
                        stats.max,
                        pct
                    );
                    println!("  Identity: {} | Runtime: {} | Custom: {} | Plan: {} | Project: {} | Memory: {} | History: {} | Current: {}", 
                        stats.system_static, stats.system_runtime, stats.system_custom, stats.system_task_plan, stats.system_project, stats.memory, stats.history, stats.current_turn);
                    println!(
                        "  Use {} for deep dive or {} to see changes.",
                        style("/context audit").bold(),
                        style("/context diff").bold()
                    );
                }
            }
            continue;
        }

        if line.starts_with("/model") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                println!(
                    "  {} Usage: /model <provider> [model_name]",
                    style("ℹ").blue()
                );
                continue;
            }
            let provider = parts[1];
            let model = parts.get(2).map(|s| s.to_string());
            match session_manager
                .update_session_llm("cli", provider, model)
                .await
            {
                Ok(msg) => println!("  {} {}", style("✔").green(), msg),
                Err(e) => println!("  {} Error updating model: {}", style("❌").red(), e),
            }
            continue;
        }

        if line.starts_with("/") {
            println!("  {} Unknown command: {}", style("❌").red(), line);
            continue;
        }

        let agent = match session_manager
            .get_or_create_session("cli", output.clone())
            .await
        {
            Ok(a) => a,
            Err(e) => {
                println!("  {} Error: {}", style("❌").red(), e);
                continue;
            }
        };

        let line = line.to_string();
        let mut agent_guard = agent.lock().await;

        let _ = output.on_waiting("Processing...").await;

        match agent_guard.step(line).await {
            Ok(exit) => match exit {
                RunExit::YieldedToUser => {
                    println!();
                }
                RunExit::Finished(ref summary) => {
                    println!("\n{}", style(summary).green().bold());
                    println!("  {} {}", style("✔").green().bold(), style("Mission accomplished. All tasks have been completed.").green());
                    println!("  {} {}", style("ℹ").blue().bold(), style("I am standing by. Please let me know if you have any new instructions.").dim());
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
                    println!(
                        "  👉 Action required: Wait for compaction or use {} to start fresh.",
                        style("/new").green()
                    );
                }
                RunExit::RecoverableFailed(ref msg) => {
                    println!("\n  {} Recoverable Failure: {}", style("⚠").yellow(), msg);
                }
                RunExit::CriticallyFailed(ref msg) => {
                    println!("\n  {} Critical Failure: {}", style("✖").red(), msg);
                    println!("  The system encountered an unrecoverable error.");
                }
            },
            Err(e) => eprintln!("  {} Agent error: {}", style("✖").red(), e),
        }
    }

    Ok(())
}
