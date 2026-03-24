use crate::core::{AgentOutput, RunExit};
use crate::session_manager::SessionManager;
use console::style;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::sync::Arc;

pub async fn run_headless_command(
    session_manager: Arc<SessionManager>,
    output: Arc<dyn AgentOutput>,
    command: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let agent = session_manager
        .get_or_create_session("headless", "cli", output.clone())
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
        if line == "/help" {
            print_help();
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
        if line.starts_with("/autopilot") {
            let goal = line.trim_start_matches("/autopilot").trim().to_string();
            let agent = session_manager
                .get_or_create_session("cli", "cli", output.clone())
                .await
                .unwrap();
            let mut agent_guard = agent.lock().await;
            agent_guard.enable_autopilot();
            println!("  {} Autopilot mode enabled.", style("🚀").green());
            if !goal.is_empty() {
                drop(agent_guard);
                run_cli_agent_step(session_manager.clone(), output.clone(), goal).await;
            }
            continue;
        }
        if line == "/manual" {
            let agent = session_manager
                .get_or_create_session("cli", "cli", output.clone())
                .await
                .unwrap();
            let mut agent_guard = agent.lock().await;
            agent_guard.is_autopilot = false;
            println!("  {} Autopilot mode disabled. Switched to manual mode.", style("✔").green());
            continue;
        }
        if line == "/status" {
            print_status(session_manager.clone(), output.clone()).await;
            continue;
        }
        if line == "/session" {
            print_sessions(session_manager.clone());
            continue;
        }
        if line.starts_with("/cron") {
            handle_cron_command(session_manager.clone(), &line).await;
            continue;
        }
        if line.starts_with("/context") {
            handle_context_command(session_manager.clone(), output.clone(), &line).await;
            continue;
        }

        if line.starts_with("/model") {
            handle_model_command(session_manager.clone(), &line).await;
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
    println!();
}

async fn print_status(session_manager: Arc<SessionManager>, output: Arc<dyn AgentOutput>) {
    let agent = session_manager
        .get_or_create_session("cli", "cli", output)
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
}

async fn handle_cron_command(session_manager: Arc<SessionManager>, line: &str) {
    let parts: Vec<&str> = line.split_whitespace().collect();
    let action = parts.get(1).copied().unwrap_or("list");
    let scheduler = crate::scheduler::Scheduler::new(session_manager.clone());

    match action {
        "list" => {
            let tasks = scheduler.list_tasks().await;
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
        "remove" => {
            if let Some(id) = parts.get(2) {
                match scheduler.remove_task(id).await {
                    Ok(_) => println!("  {} Task '{}' removed.", style("✔").green(), id),
                    Err(e) => println!("  {} Error: {}", style("❌").red(), e),
                }
            } else {
                println!("  {} Usage: /cron remove <id>", style("ℹ").blue());
            }
        }
        "toggle" => {
            if let (Some(id), Some(state)) = (parts.get(2), parts.get(3)) {
                let enabled = match *state {
                    "on" | "true" | "enable" => true,
                    "off" | "false" | "disable" => false,
                    _ => {
                        println!(
                            "  {} Usage: /cron toggle <id> <on|off>",
                            style("ℹ").blue()
                        );
                        return;
                    }
                };
                match scheduler.toggle_task(id, enabled).await {
                    Ok(_) => println!("  {} Task '{}' is now {}.", style("✔").green(), id, if enabled { "enabled" } else { "disabled" }),
                    Err(e) => println!("  {} Error: {}", style("❌").red(), e),
                }
            } else {
                println!("  {} Usage: /cron toggle <id> <on|off>", style("ℹ").blue());
            }
        }
        _ => {
            println!("  {} Unknown action. Use: list, remove, toggle", style("❌").red());
        }
    }
}

fn print_sessions(session_manager: Arc<SessionManager>) {
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
}

async fn handle_context_command(
    session_manager: Arc<SessionManager>,
    output: Arc<dyn AgentOutput>,
    line: &str,
) {
    let agent = session_manager
        .get_or_create_session("cli", "cli", output)
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
            println!(
                "  Identity: {} | Runtime: {} | Custom: {} | Plan: {} | Project: {} | Memory: {} | History: {} | Current: {}",
                stats.system_static,
                stats.system_runtime,
                stats.system_custom,
                stats.system_task_plan,
                stats.system_project,
                stats.memory,
                stats.history,
                stats.current_turn
            );
            println!(
                "  Use {} for deep dive or {} to see changes.",
                style("/context audit").bold(),
                style("/context diff").bold()
            );
        }
    }
}

async fn handle_model_command(session_manager: Arc<SessionManager>, line: &str) {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        println!(
            "  {} Usage: /model <provider> [model_name]",
            style("ℹ").blue()
        );
        return;
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
                    println!("  {} {}", style("Autopilot Paused").yellow().bold(), style("任务已安全暂停。").yellow());
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
                println!("  👉 Autopilot 未检测到有效进展，已暂停。您可以输入指导意见并继续，或输入 /manual 退出自动驾驶。");
            }
        },
        Err(e) => eprintln!("  {} Agent error: {}", style("✖").red(), e),
    }
}
