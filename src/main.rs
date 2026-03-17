#[cfg(feature = "acp")]
mod acp;
mod app;
#[cfg(test)]
mod browser;
mod context_assembler;
#[cfg(test)]
mod event_log;
mod evidence;
mod lsp_client;
mod rag;
mod schema;
mod task_state;
mod telemetry;

mod config;
mod context;
mod core;
mod discord;
mod llm_client;
mod logging;
mod memory;
mod session;
mod session_manager;
mod skills;
mod telegram;
mod tools;
mod ui;
mod utils;

use crate::session_manager::SessionManager;
use crate::ui::TuiOutput;
use clap::Parser;
use console::style;
use dotenvy::dotenv;
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
    /// Execute a single command and exit (headless mode)
    #[arg(long, short = 'c')]
    command: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();
    let args = CliArgs::parse();

    let is_headless = args.command.is_some();

    if !is_headless {
        println!();
        println!("  {}", style("Rusty-Claw AGENT-OS v0.1.0").bold().cyan());
        println!("  {}", style("---------------------------").dim());
    }

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
            if !is_headless {
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
            }
            None
        }
    };

    let bootstrap = app::bootstrap::build_app_bootstrap()?;
    let telegram_token = std::env::var("TELEGRAM_BOT_TOKEN").ok();

    let session_manager = Arc::new(SessionManager::new(llm_opt, bootstrap.tools.clone()));
    let output = Arc::new(TuiOutput::new());

    if !is_headless {
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
    }

    if let Some(cmd) = args.command {
        app::cli::run_headless_command(session_manager.clone(), output.clone(), cmd).await?;
        return Ok(());
    }

    app::cli::run_cli_repl(session_manager, output).await?;

    Ok(())
}
