use super::output::TelegramOutput;
use super::Command;
use crate::core::{AgentOutput, RunExit};
use crate::session_manager::SessionManager;
use std::sync::Arc;
use teloxide::{net::Download, prelude::*, types::ParseMode, utils::command::BotCommands};

pub(super) async fn handle_callback_query(
    bot: Bot,
    q: CallbackQuery,
    session_manager: Arc<SessionManager>,
) -> ResponseResult<()> {
    if let Some(data) = q.data {
        if data == "cancel_task" {
            let chat_id = q.message.map(|m| m.chat().id);
            if let Some(cid) = chat_id {
                let session_id = format!("telegram:{}", cid);
                session_manager.cancel_session(&session_id).await;
                let _ = bot
                    .answer_callback_query(q.id)
                    .text("🛑 正在请求停止任务...")
                    .await;
                let _ = bot
                    .send_message(cid, "🛑 正在尝试中止当前任务进程...")
                    .await;
            }
        }
    }
    Ok(())
}

pub(super) async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    session_manager: Arc<SessionManager>,
) -> ResponseResult<()> {
    let chat_id = msg.chat.id;
    let session_id = format!("telegram:{}", chat_id);

    match cmd {
        Command::Help => {
            bot.send_message(chat_id, Command::descriptions().to_string())
                .await?;
        }
        Command::Reset => {
            session_manager.reset_session(&session_id).await;
            bot.send_message(chat_id, "♻️ Session reset.").await?;
        }
        Command::Ping => {
            bot.send_message(chat_id, "🏓 Pong!").await?;
        }
        Command::Cancel => {
            session_manager.cancel_session(&session_id).await;
            bot.send_message(chat_id, "🛑 Task cancellation requested.")
                .await?;
        }
        Command::Status => {
            let output = Arc::new(TelegramOutput::new(bot.clone(), chat_id));
            let agent = match session_manager
                .get_or_create_session(&session_id, &session_id, output.clone())
                .await
            {
                Ok(a) => a,
                Err(e) => {
                    bot.send_message(chat_id, format!("❌ Error: {}", e))
                        .await?;
                    return Ok(());
                }
            };

            let bot_clone = bot.clone();
            tokio::spawn(async move {
                let mut agent_guard =
                    match tokio::time::timeout(std::time::Duration::from_secs(3), agent.lock())
                        .await
                    {
                        Ok(guard) => guard,
                        Err(_) => {
                            let _ = bot_clone
                                .send_message(chat_id, "⏳ 状态获取超时 (Agnet 繁忙)")
                                .await;
                            return;
                        }
                    };

                agent_guard.flush_output().await;
                agent_guard.update_output(output);

                let (provider, model, tokens, max_tokens) = agent_guard.get_status();
                let usage_pc = if max_tokens > 0 {
                    (tokens as f64 / max_tokens as f64 * 100.0) as usize
                } else {
                    0
                };
                let bar_len = 10;
                let filled = (usage_pc * bar_len) / 100;
                let bar = format!("{}{}", "▓".repeat(filled), "░".repeat(bar_len - filled));

                let mut status_msg = format!(
                    "🤖 *System Status*\n\
                    ━━━━━━━━━━━━━━━━━━━━━\n\
                    *LLM*: {} / {}\n\
                    *Context*: {} / {} tokens\n\
                    `[{}]` {}%\n\n",
                    TelegramOutput::escape_markdown_v2(&provider),
                    TelegramOutput::escape_markdown_v2(&model),
                    tokens,
                    max_tokens,
                    bar,
                    usage_pc
                );

                let ts = crate::task_state::TaskStateStore::new(&session_id);
                if ts.has_active_plan() {
                    if let Ok(state) = ts.load() {
                        let total_steps = state.plan_steps.len();
                        let completed_steps = state
                            .plan_steps
                            .iter()
                            .filter(|s| s.status == "completed")
                            .count();
                        let current_step =
                            state.plan_steps.iter().find(|s| s.status == "in_progress");

                        status_msg.push_str(&format!(
                            "🎯 *Active Plan*: {}%\n\
                            *Goal*: {}\nProgress: {} / {} steps\n",
                            if total_steps > 0 {
                                completed_steps * 100 / total_steps
                            } else {
                                0
                            },
                            TelegramOutput::escape_markdown_v2(
                                &state.goal.unwrap_or_else(|| "Unknown".to_string())
                            ),
                            completed_steps,
                            total_steps
                        ));

                        if let Some(step) = current_step {
                            status_msg.push_str(&format!(
                                "👉 *Now*: {}\n",
                                TelegramOutput::escape_markdown_v2(&step.step)
                            ));
                        }

                        status_msg.push_str("\n💡 Say \"continue\" or use /cancel\\.");
                    }
                } else {
                    status_msg.push_str("⚪ *No active plan*\\. Ready for new tasks\\.");
                }

                let _ = bot_clone
                    .send_message(chat_id, status_msg)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await;
            });
        }
        Command::Session => {
            let output = Arc::new(TelegramOutput::new(bot.clone(), chat_id));
            let agent = match session_manager
                .get_or_create_session(&session_id, &session_id, output.clone())
                .await
            {
                Ok(a) => a,
                Err(e) => {
                    bot.send_message(chat_id, format!("❌ Error: {}", e))
                        .await?;
                    return Ok(());
                }
            };

            let bot_clone = bot.clone();
            tokio::spawn(async move {
                let mut agent_guard =
                    match tokio::time::timeout(std::time::Duration::from_secs(3), agent.lock())
                        .await
                    {
                        Ok(guard) => guard,
                        Err(_) => {
                            let _ = bot_clone
                                .send_message(chat_id, "⏳ 会话状态获取超时 (Agnet 繁忙)")
                                .await;
                            return;
                        }
                    };

                agent_guard.flush_output().await;
                agent_guard.update_output(output);

                let details = agent_guard.get_session_details();

                let formatted = format!(
                    "📝 *Detailed Session Diagnostics*\n\
                    ━━━━━━━━━━━━━━━━━━━━━\n\
                    *ID*: `{}`\n\
                    *LLM*: `{}` / `{}`\n\
                    *Task*: `{}` ({})\n\
                    *Context*: {} / {} tokens\n\
                    *Turns*: `{}`\n\
                    *System Prompts*: `{}`\n\
                    *Active Evidence*: `{}`\n\
                    *Cancelled*: `{}`",
                    TelegramOutput::escape_markdown_v2(
                        details["session_id"].as_str().unwrap_or("")
                    ),
                    TelegramOutput::escape_markdown_v2(details["provider"].as_str().unwrap_or("")),
                    TelegramOutput::escape_markdown_v2(
                        details["model"].as_str().unwrap_or("unknown")
                    ),
                    TelegramOutput::escape_markdown_v2(
                        details["task_id"].as_str().unwrap_or("none")
                    ),
                    details["task_status"].as_str().unwrap_or("idle"),
                    details["context"]["tokens"],
                    details["context"]["max_tokens"],
                    details["context"]["turns"],
                    details["context"]["system_tokens"],
                    details["context"]["active_evidence"],
                    details["cancelled"]
                );
                let _ = bot_clone
                    .send_message(chat_id, formatted)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await;
            });
        }
        Command::Model(args) => {
            let parts: Vec<&str> = args.split_whitespace().collect();
            if parts.is_empty() {
                bot.send_message(chat_id, "Usage: /model <provider> [model_name]")
                    .await?;
                return Ok(());
            }

            let provider = parts[0];
            let model = parts.get(1).map(|s| s.to_string());

            match session_manager
                .update_session_llm(&session_id, provider, model)
                .await
            {
                Ok(msg) => {
                    bot.send_message(chat_id, format!("✅ {}", msg)).await?;
                }
                Err(e) => {
                    bot.send_message(chat_id, format!("❌ Error: {}", e))
                        .await?;
                }
            }
        }
        Command::Cron(args) => {
            let parts: Vec<&str> = args.split_whitespace().collect();
            let action = parts.get(0).copied().unwrap_or("list");
            let scheduler = Arc::new(crate::scheduler::Scheduler::new(session_manager.clone()));

            match action {
                "list" => {
                    let tasks = scheduler.list_tasks().await;
                    if tasks.is_empty() {
                        bot.send_message(chat_id, "⚪ No scheduled tasks found.").await?;
                    } else {
                        let mut msg = "📅 *Scheduled Tasks*\n━━━━━━━━━━━━━━━━━━━━━\n".to_string();
                        for task in tasks {
                            let status = if task.enabled { "✅" } else { "❌" };
                            msg.push_str(&format!(
                                "*ID*: `{}` {}\n*Cron*: `{}`\n*Goal*: {}\n\n",
                                TelegramOutput::escape_markdown_v2(&task.id),
                                status,
                                TelegramOutput::escape_markdown_v2(&task.cron),
                                TelegramOutput::escape_markdown_v2(&task.goal)
                            ));
                        }
                        bot.send_message(chat_id, msg)
                            .parse_mode(ParseMode::MarkdownV2)
                            .await?;
                    }
                }
                "remove" => {
                    if let Some(id) = parts.get(1) {
                        match scheduler.remove_task(id).await {
                            Ok(_) => {
                                bot.send_message(chat_id, format!("✅ Task `{}` removed.", id)).await?;
                            }
                            Err(e) => {
                                bot.send_message(chat_id, format!("❌ Error: {}", e)).await?;
                            }
                        }
                    } else {
                        bot.send_message(chat_id, "Usage: /cron remove <id>").await?;
                    }
                }
                "toggle" => {
                    if let (Some(id), Some(state)) = (parts.get(1), parts.get(2)) {
                        let enabled = match *state {
                            "on" | "true" | "enable" => true,
                            "off" | "false" | "disable" => false,
                            _ => {
                                bot.send_message(chat_id, "Usage: /cron toggle <id> <on|off>").await?;
                                return Ok(());
                            }
                        };
                        match scheduler.toggle_task(id, enabled).await {
                            Ok(_) => {
                                bot.send_message(chat_id, format!("✅ Task `{}` is now {}.", id, if enabled { "enabled" } else { "disabled" })).await?;
                            }
                            Err(e) => {
                                bot.send_message(chat_id, format!("❌ Error: {}", e)).await?;
                            }
                        }
                    } else {
                        bot.send_message(chat_id, "Usage: /cron toggle <id> <on|off>").await?;
                    }
                }
                _ => {
                    bot.send_message(chat_id, "Unknown action. Use: list, remove, toggle").await?;
                }
            }
        }
    }

    Ok(())
}

pub(super) async fn handle_message(
    bot: Bot,
    msg: Message,
    session_manager: Arc<SessionManager>,
) -> ResponseResult<()> {
    tracing::info!("Telegram: Received message from {}", msg.chat.id);
    let mut final_text = String::new();

    if let Some(text) = msg.text() {
        final_text = text.to_string();
    } else if let Some(caption) = msg.caption() {
        final_text = caption.to_string();
    }

    if let Some(photos) = msg.photo() {
        if let Some(photo) = photos.last() {
            if let Ok(file) = bot.get_file(&photo.file.id).await {
                let temp_dir = std::env::temp_dir();
                let path = temp_dir.join(format!("{}.jpg", photo.file.id));
                if let Ok(mut dest) = tokio::fs::File::create(&path).await {
                    if bot.download_file(&file.path, &mut dest).await.is_ok() {
                        let img_msg = format!(
                            "[User uploaded an image. Saved locally at: {}]",
                            path.display()
                        );
                        if final_text.is_empty() {
                            final_text = img_msg;
                        } else {
                            final_text = format!("{}\n{}", final_text, img_msg);
                        }
                    }
                }
            }
        }
    }

    if !final_text.is_empty() {
        let text = final_text;
        let chat_id = msg.chat.id;
        let session_id = format!("telegram:{}", chat_id);

        if text == "🛑" || text == "🆘" || text.to_lowercase() == "stop" {
            session_manager.cancel_session(&session_id).await;
            bot.send_message(chat_id, "🛑 接收到紧急停止指令。").await?;
            return Ok(());
        }

        let output = Arc::new(TelegramOutput::new(bot.clone(), chat_id));

        let agent = match session_manager
            .get_or_create_session(&session_id, &session_id, output.clone())
            .await
        {
            Ok(a) => a,
            Err(e) => {
                bot.send_message(chat_id, format!("❌ Error: {}", e))
                    .await?;
                return Ok(());
            }
        };

        let bot_clone = bot.clone();
        tokio::spawn(async move {
            let mut agent_guard = match tokio::time::timeout(
                std::time::Duration::from_secs(3),
                agent.lock(),
            )
            .await
            {
                Ok(guard) => guard,
                Err(_) => {
                    let _ = bot_clone
                            .send_message(
                                chat_id,
                                "⏳ 上一个任务仍在执行中，请等待完成后再发送新消息，或使用 /cancel 取消当前任务。",
                            )
                            .await;
                    return;
                }
            };

            agent_guard.flush_output().await;
            agent_guard.update_output(output.clone());

            let ts = crate::task_state::TaskStateStore::new(&session_id);
            if ts.has_active_plan() && text.to_lowercase() != "continue" && !text.starts_with('/') {
                static LAST_REMINDED: once_cell::sync::Lazy<
                    std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
                > = once_cell::sync::Lazy::new(|| {
                    std::sync::Mutex::new(std::collections::HashMap::new())
                });

                let mut should_remind = false;
                {
                    let mut map = LAST_REMINDED.lock().unwrap();
                    if let Some(&last) = map.get(&session_id) {
                        if last.elapsed() > std::time::Duration::from_secs(3600) {
                            should_remind = true;
                            map.insert(session_id.clone(), std::time::Instant::now());
                        }
                    } else {
                        should_remind = true;
                        map.insert(session_id.clone(), std::time::Instant::now());
                    }
                }

                if should_remind {
                    if let Ok(state) = ts.load() {
                        let task_msg = format!(
                            "🎯 *Active Task Reminder*\nTask: {}\n\n💡 You can say \"continue\" to proceed with this task, or use /cancel to abort\\.",
                            TelegramOutput::escape_markdown_v2(
                                &state.goal.unwrap_or_else(|| "Unknown".to_string())
                            )
                        );
                        let _ = bot_clone
                            .send_message(chat_id, task_msg)
                            .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                            .await;
                    }
                }
            }

            let bot_typing = bot_clone.clone();
            let typing_done = Arc::new(tokio::sync::Notify::new());
            let typing_done_clone = typing_done.clone();

            tokio::spawn(async move {
                loop {
                    let _ = bot_typing
                        .send_chat_action(chat_id, teloxide::types::ChatAction::Typing)
                        .await;
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {},
                        _ = typing_done_clone.notified() => break,
                    }
                }
            });

            let _ = output.on_waiting("Processing...").await;

            let result = agent_guard.step(text).await;
            drop(agent_guard);

            typing_done.notify_one();

            match result {
                Ok(exit) => match exit {
                    RunExit::AgentTurnLimitReached => {
                        let _ = bot_clone.send_message(chat_id, "⚠️ [Turn Limit Reached] The agent reached the maximum allowed consecutive actions. Please type 'continue' if you want it to proceed.").await;
                    }
                    RunExit::RecoverableFailed(ref msg) | RunExit::CriticallyFailed(ref msg) => {
                        let _ = bot_clone
                            .send_message(
                                chat_id,
                                format!("⚠️ Run stopped: {}\nReason: {}", exit.label(), msg),
                            )
                            .await;
                    }
                    RunExit::StoppedByUser => {
                        let _ = bot_clone
                            .send_message(chat_id, "✅ 任务已手动中止。随时可以开始新任务。")
                            .await;
                    }
                    _ => {}
                },
                Err(e) => {
                    let _ = bot_clone
                        .send_message(chat_id, format!("❌ Error: {}", e))
                        .await;
                }
            }
        });
    }
    Ok(())
}
