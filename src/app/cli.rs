use crate::app::commands::{Command, CommandExecutor, CommandOutput, StatusData};
use crate::core::{AgentOutput, RunExit};
use crate::session_manager::SessionManager;
use console::style;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::sync::Arc;

pub struct CliCommandOutput;

impl CommandOutput for CliCommandOutput {
    fn send_text(&self, text: &str) {
        println!("{}", text);
    }

    fn send_error(&self, error: &str) {
        println!("  {} Error: {}", style("❌").red(), error);
    }

    fn send_success(&self, message: &str) {
        println!("  {} {}", style("✔").green(), message);
    }

    fn send_status(&self, data: StatusData) {
        let percentage = (data.tokens as f64 / data.max_tokens as f64) * 100.0;
        println!(
            "  {} Provider: {}, Model: {}, Context: {}/{} tokens ({:.1}%)",
            style("📊").cyan(),
            data.provider,
            data.model,
            data.tokens,
            data.max_tokens,
            percentage
        );
        if let Some(state) = data.active_plan {
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

    fn send_session_list(&self, sessions: Vec<(String, u64, usize)>) {
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
    }

    fn send_cron_list(&self, tasks: Vec<crate::scheduler::ScheduledTask>) {
        if tasks.is_empty() {
            println!("  {} No scheduled tasks found.", style("⚪").dim());
        } else {
            println!("\n  {}", style("Scheduled Tasks:").bold());
            for task in tasks {
                let status = if task.enabled {
                    style("enabled").green()
                } else {
                    style("disabled").red()
                };
                println!(
                    "  • {} [{}] - {} ({})",
                    style(&task.id).bold().cyan(),
                    status,
                    task.goal,
                    style(&task.cron).dim()
                );
            }
            println!();
        }
    }

    fn send_context_audit(&self, details: String) {
        if details.is_empty() {
            // Default view logic moved to executor or handled here
            // For now, we just print what we get
        } else {
            println!("{}", details);
        }
    }

    fn send_context_diff(&self, diff: Option<String>) {
        if let Some(diff) = diff {
            println!("{}", diff);
        } else {
            println!("  {} No changes since last snapshot.", style("ℹ").blue());
        }
    }

    fn send_context_inspect(&self, result: String) {
        println!("{}", result);
    }

    fn send_context_dump(&self, path: String) {
        println!("  {} Context dumped to {}", style("✔").green(), path);
    }

    fn send_context_compact(&self, result: Result<(), String>) {
        match result {
            Ok(_) => println!("  {} Compaction attempt finished.", style("✔").green()),
            Err(e) => println!("  {} Compaction failed: {}", style("❌").red(), e),
        }
    }

    fn send_trace(&self, trace: String) {
        println!("\n{}\n", trace);
    }
}

pub async fn run_headless_command(
    session_manager: Arc<SessionManager>,
    output: Arc<dyn AgentOutput>,
    command: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let session_id = format!("cli_headless_{}", uuid::Uuid::new_v4().simple());
    let agent = session_manager
        .get_or_create_session(&session_id, "cli", output.clone())
        .await?;

    let mut agent_guard = agent.lock().await;
    let _ = output.on_waiting("Processing headless command...").await;

    match agent_guard.step(command).await {
        Ok(exit) => match exit {
            RunExit::Finished(summary) => {
                println!("\n{}", style(summary).green().bold());
            }
            RunExit::RecoverableFailed(msg) => {
                eprintln!("\n  {} Error: {}", style("⚠️").yellow(), msg);
                std::process::exit(1);
            }
            RunExit::CriticallyFailed(msg) => {
                eprintln!("\n  {} Critical Error: {}", style("✖").red(), msg);
                std::process::exit(1);
            }
            _ => {
                println!("\n  Execution ended with status: {:?}", exit);
            }
        },
        Err(e) => {
            eprintln!("  {} Agent error: {}", style("✖").red(), e);
            std::process::exit(1);
        }
    }

    Ok(())
}

pub async fn run_cli_repl(
    session_manager: Arc<SessionManager>,
    output: Arc<dyn AgentOutput>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut rl = DefaultEditor::new()?;
    let executor = CommandExecutor::new(session_manager.clone());
    let cmd_output = Arc::new(CliCommandOutput);

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

        rl.add_history_entry(&current_input).ok();
        current_input.clear();

        if line == "/exit" {
            break;
        }

        if let Some(cmd) = Command::parse(&line) {
            if matches!(cmd, Command::Help) {
                print_help();
                continue;
            }

            if let Err(e) = executor
                .execute("cli", "cli", output.clone(), cmd_output.clone(), cmd)
                .await
            {
                cmd_output.send_error(&e);
            }
            continue;
        }

        if line.starts_with('/') {
            println!("  {} Unknown command: {}", style("❌").red(), line);
            continue;
        }

        run_cli_agent_step(session_manager.clone(), output.clone(), line).await;
    }

    Ok(())
}

fn print_help() {
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
    println!("  {} - Enable autopilot mode", style("/autopilot").green());
    println!("  {} - Switch to manual mode", style("/manual").yellow());
    println!("  {} - List all sessions", style("/session").white());
    println!("  {} - Manage scheduled tasks", style("/cron").yellow());
    println!("  {} - Inspect context", style("/context").blue());
    println!("  {} - Trace subagent execution", style("/trace <job_id>").magenta());

    let mut registry = crate::skills::registry::SkillRegistry::new();
    registry.discover(std::path::Path::new("skills"));
    let names = registry.names();
    if !names.is_empty() {
        println!();
        println!("  {}", style("Available Skills:").bold());
        for name in names {
            if let Some(skill) = registry.clone_skill(name) {
                let desc = skill.meta.description.lines().next().unwrap_or("").trim();
                let short_desc = if desc.chars().count() > 60 {
                    format!("{}...", desc.chars().take(57).collect::<String>())
                } else {
                    desc.to_string()
                };
                println!("  {} - {}", style(format!("/{}", name)).cyan(), short_desc);
            }
        }
    }

    println!();
}

async fn run_cli_agent_step(
    session_manager: Arc<SessionManager>,
    output: Arc<dyn AgentOutput>,
    line: String,
) {
    let agent = match session_manager
        .get_or_create_session("cli", "cli", output.clone())
        .await
    {
        Ok(a) => a,
        Err(e) => {
            println!("  {} Error: {}", style("❌").red(), e);
            return;
        }
    };

    let mut agent_guard = agent.lock().await;
    let _ = output.on_waiting("Processing...").await;

    match agent_guard.step(line).await {
        Ok(exit) => match exit {
            RunExit::YieldedToUser => {
                println!();
            }
            RunExit::Finished(ref summary) => {
                println!("\n{}", style(summary).green().bold());
                println!(
                    "  {} {}",
                    style("✔").green().bold(),
                    style("Mission accomplished. All tasks have been completed.").green()
                );
                println!(
                    "  {} {}",
                    style("ℹ").blue().bold(),
                    style("I am standing by. Please let me know if you have any new instructions.")
                        .dim()
                );
            }
            RunExit::StoppedByUser => {
                println!("\n  {}", style("Execution Stopped by User").yellow());
                println!("  The current operation was manually cancelled.");
                if agent_guard.is_autopilot {
                    println!(
                        "  {} {}",
                        style("Autopilot Paused").yellow().bold(),
                        style("任务已安全暂停。").yellow()
                    );
                    println!("  👉 您可以直接输入指导意见来纠偏并自动继续，或者输入 /manual 彻底退出自动驾驶。");
                }
            }
            RunExit::RecoverableFailed(ref msg) => {
                println!("\n  {} Recoverable Failure: {}", style("⚠").yellow(), msg);
            }
            RunExit::CriticallyFailed(ref msg) => {
                println!("\n  {} Critical Failure: {}", style("✖").red(), msg);
                println!("  The system encountered an unrecoverable error.");
            }
            RunExit::AutopilotStalled(ref msg) => {
                println!("\n  {} Autopilot 停滞: {}", style("⚠").yellow(), msg);
                println!("  👉 Autopilot 未检测到有效进展，已暂停。您可以���入指导意见并继续，或输入 /manual 退出自动驾驶。");
            }
        },
        Err(e) => eprintln!("  {} Agent error: {}", style("✖").red(), e),
    }
}
