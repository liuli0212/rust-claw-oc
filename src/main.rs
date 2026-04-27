use clap::Parser;
use console::style;
use dotenvy::dotenv;
use rusty_claw::app;
use rusty_claw::config;
use rusty_claw::discord;
use rusty_claw::llm_client;
use rusty_claw::logging;
use rusty_claw::scheduler;
use rusty_claw::session_manager::SessionManager;
use rusty_claw::telegram;
use rusty_claw::tools;
use rusty_claw::ui::{TuiOutput, TuiOutputRouter};
use std::sync::Arc;
use std::time::Duration;

const LOGO: &str = r#"
  ██████╗ ██╗   ██╗███████╗████████╗██╗   ██╗      ██████╗██╗      █████╗ ██╗    ██╗
  ██╔══██╗██║   ██║██╔════╝╚══██╔══╝╚██╗ ██╔╝     ██╔════╝██║     ██╔══██╗██║    ██║
  ██████╔╝██║   ██║███████╗   ██║    ╚████╔╝  ████╗██║     ██║     ███████║██║ █╗ ██║
  ██╔══██╗██║   ██║╚════██║   ██║     ╚██╔╝   ╚═══╝██║     ██║     ██╔══██║██║███╗██║
  ██║  ██║╚██████╔╝███████║   ██║      ██║          ╚██████╗███████╗██║  ██║╚███╔███╔╝
  ╚═╝  ╚═╝ ╚═════╝ ╚══════╝   ╚═╝      ╚═╝           ╚═════╝╚══════╝╚═╝  ╚═╝ ╚══╝╚══╝
"#;

fn styles() -> clap::builder::styling::Styles {
    use clap::builder::styling::{AnsiColor, Effects, Styles};
    Styles::styled()
        .header(AnsiColor::Yellow.on_default() | Effects::BOLD)
        .usage(AnsiColor::Yellow.on_default() | Effects::BOLD)
        .literal(AnsiColor::Blue.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Green.on_default())
}

#[derive(Debug, Parser)]
#[command(
    name = "rusty-claw",
    about = "Rusty-Claw CLI agent",
    before_help = LOGO,
    styles = styles()
)]
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
    /// Code mode prompt/dispatch format: function or text
    #[arg(long, default_value = "function", value_parser = ["function", "text"])]
    code_mode_format: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async_main());

    // Code mode uses spawn_blocking for the JS runtime. If a worker fails to
    // observe cancellation promptly, Tokio's default shutdown would wait
    // indefinitely and leave the CLI stuck after printing "Exiting...".
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}

async fn async_main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();
    let args = CliArgs::parse();

    let is_headless = args.command.is_some();

    if !is_headless {
        println!();
        println!("{}", style(LOGO).cyan());
        println!("  {}", style("Rusty-Claw AGENT-OS v0.1.0").bold().cyan());
        println!("  {}", style("---------------------------").dim());
    }

    let config = config::AppConfig::load();
    let _guards = logging::init_logging(&config);
    let code_mode_format = args
        .code_mode_format
        .parse::<rusty_claw::code_mode::description::CodeModeFormat>()
        .expect("clap validates code_mode_format");

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

    let session_manager = Arc::new(SessionManager::new_with_code_mode_format(
        llm_opt,
        bootstrap.tools.clone(),
        code_mode_format,
    ));
    session_manager.add_output_router(Arc::new(TuiOutputRouter));
    let output = Arc::new(TuiOutput::new());

    // Initialize and start the scheduler
    let scheduler_path = std::path::PathBuf::from("rusty_claw").join("schedule.json");
    let scheduler = Arc::new(scheduler::Scheduler::new(
        session_manager.clone(),
        scheduler_path,
    ));
    session_manager.set_scheduler(scheduler.clone());
    let scheduler_clone = scheduler.clone();
    tokio::spawn(async move {
        scheduler_clone.run_loop().await;
    });
    // Note: Registering the tool here is safe because no sessions have been created yet.
    // Sessions snapshot the tool list upon creation.
    session_manager.add_tool(Arc::new(tools::ManageScheduleTool { scheduler }));

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
                    let acp_server = rusty_claw::acp::AcpServer::new(sm);
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
