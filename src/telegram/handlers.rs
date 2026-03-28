use super::output::TelegramOutput;
use super::Command as TgCommand;
use crate::app::commands::{Command, CommandExecutor, CommandOutput, StatusData};
use crate::core::{AgentOutput, RunExit};
use crate::session_manager::SessionManager;
use std::sync::Arc;
use teloxide::{prelude::*, types::ParseMode, utils::command::BotCommands, net::Download};

pub struct TelegramCommandOutput {
    bot: Bot,
    chat_id: ChatId,
}

impl TelegramCommandOutput {
    pub fn new(bot: Bot, chat_id: ChatId) -> Self {
        Self { bot, chat_id }
    }

    fn escape(text: &str) -> String {
        TelegramOutput::escape_markdown_v2(text)
    }
}

impl CommandOutput for TelegramCommandOutput {
    fn send_text(&self, text: &str) {
        let bot = self.bot.clone();
        let chat_id = self.chat_id;
        let text = text.to_string();
        tokio::spawn(async move {
            let _ = bot.send_message(chat_id, text).await;
        });
    }

    fn send_error(&self, error: &str) {
        let bot = self.bot.clone();
        let chat_id = self.chat_id;
        let msg = format!("❌ Error: {}", error);
        tokio::spawn(async move {
            let _ = bot.send_message(chat_id, msg).await;
        });
    }

    fn send_success(&self, message: &str) {
        let bot = self.bot.clone();
        let chat_id = self.chat_id;
        let msg = format!("✅ {}", message);
        tokio::spawn(async move {
            let _ = bot.send_message(chat_id, msg).await;
        });
    }

    fn send_status(&self, data: StatusData) {
        let bot = self.bot.clone();
        let chat_id = self.chat_id;
        tokio::spawn(async move {
            let usage_pc = if data.max_tokens > 0 {
                (data.tokens as f64 / data.max_tokens as f64 * 100.0) as usize
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
                Self::escape(&data.provider),
                Self::escape(&data.model),
                data.tokens,
                data.max_tokens,
                bar,
                usage_pc
            );

            if let Some(state) = data.active_plan {
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
                    Self::escape(
                        &state.goal.unwrap_or_else(|| "Unknown".to_string())
                    ),
                    completed_steps,
                    total_steps
                ));

                if let Some(step) = current_step {
                    status_msg.push_str(&format!(
                        "👉 *Now*: {}\n",
                        Self::escape(&step.step)
                    ));
                }

                status_msg.push_str("\n💡 Say \"continue\" or use /cancel\\.");
            } else {
                status_msg.push_str("⚪ *No active plan*\\. Ready for new tasks\\.");
            }

            let _ = bot
                .send_message(chat_id, status_msg)
                .parse_mode(ParseMode::MarkdownV2)
                .await;
        });
    }

    fn send_session_list(&self, sessions: Vec<(String, u64, usize)>) {
        let bot = self.bot.clone();
        let chat_id = self.chat_id;
        tokio::spawn(async move {
            let mut msg = "📝 *Active/Recent Sessions*\n━━━━━━━━━━━━━━━━━━━━━\n".to_string();
            if sessions.is_empty() {
                msg.push_str("(No sessions found)");
            } else {
                for (id, updated, turns) in sessions {
                    let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(updated);
                    let datetime: chrono::DateTime<chrono::Local> = chrono::DateTime::from(time);
                    msg.push_str(&format!(
                        "• `{}` (Turns: {}, Updated: {})\n",
                        Self::escape(&id),
                        turns,
                        Self::escape(&datetime.format("%Y-%m-%d %H:%M:%S").to_string())
                    ));
                }
            }
            let _ = bot.send_message(chat_id, msg).parse_mode(ParseMode::MarkdownV2).await;
        });
    }

    fn send_cron_list(&self, tasks: Vec<crate::scheduler::ScheduledTask>) {
        let bot = self.bot.clone();
        let chat_id = self.chat_id;
        tokio::spawn(async move {
            if tasks.is_empty() {
                let _ = bot.send_message(chat_id, "⚪ No scheduled tasks found.").await;
            } else {
                let mut msg = "📅 *Scheduled Tasks*\n━━━━━━━━━━━━━━━━━━━━━\n".to_string();
                for task in tasks {
                    let status = if task.enabled { "✅" } else { "❌" };
                    msg.push_str(&format!(
                        "*ID*: `{}` {}\n*Cron*: `{}`\n*Goal*: {}\n\n",
                        Self::escape(&task.id),
                        status,
                        Self::escape(&task.cron),
                        Self::escape(&task.goal)
                    ));
                }
                let _ = bot.send_message(chat_id, msg)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await;
            }
        });
    }

    fn send_context_audit(&self, details: String) {
        let bot = self.bot.clone();
        let chat_id = self.chat_id;
        tokio::spawn(async move {
            let _ = bot.send_message(chat_id, details).await;
        });
    }

    fn send_context_diff(&self, diff: Option<String>) {
        let bot = self.bot.clone();
        let chat_id = self.chat_id;
        tokio::spawn(async move {
            if let Some(diff) = diff {
                let _ = bot.send_message(chat_id, diff).await;
            } else {
                let _ = bot.send_message(chat_id, "ℹ️ No changes since last snapshot.").await;
            }
        });
    }

    fn send_context_inspect(&self, result: String) {
        let bot = self.bot.clone();
        let chat_id = self.chat_id;
        tokio::spawn(async move {
            let _ = bot.send_message(chat_id, result).await;
        });
    }

    fn send_context_dump(&self, path: String) {
        let bot = self.bot.clone();
        let chat_id = self.chat_id;
        tokio::spawn(async move {
            let _ = bot.send_message(chat_id, format!("✅ Context dumped locally to {}", path)).await;
        });
    }

    fn send_context_compact(&self, result: Result<(), String>) {
        let bot = self.bot.clone();
        let chat_id = self.chat_id;
        tokio::spawn(async move {
            match result {
                Ok(_) => { let _ = bot.send_message(chat_id, "✅ Compaction finished.").await; }
                Err(e) => { let _ = bot.send_message(chat_id, format!("❌ Compaction failed: {}", e)).await; }
            }
        });
    }

    fn send_trace(&self, trace: String) {
        let bot = self.bot.clone();
        let chat_id = self.chat_id;
        tokio::spawn(async move {
            let _ = bot.send_message(chat_id, trace).await;
        });
    }
}

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
    tg_cmd: TgCommand,
    session_manager: Arc<SessionManager>,
) -> ResponseResult<()> {
    let chat_id = msg.chat.id;
    let session_id = format!("telegram:{}", chat_id);
    let executor = CommandExecutor::new(session_manager.clone());
    let cmd_output = Arc::new(TelegramCommandOutput::new(bot.clone(), chat_id));
    let agent_output = Arc::new(TelegramOutput::new(bot.clone(), chat_id));

    let cmd = match tg_cmd {
        TgCommand::Help => {
            bot.send_message(chat_id, TgCommand::descriptions().to_string()).await?;
            return Ok(());
        }
        TgCommand::New => Command::New,
        TgCommand::Ping => {
            bot.send_message(chat_id, "🏓 Pong!").await?;
            return Ok(());
        }
        TgCommand::Cancel => Command::Cancel,
        TgCommand::Status => Command::Status,
        TgCommand::Session => Command::Session,
        TgCommand::Model(args) => Command::Model(args),
        TgCommand::Cron(args) => Command::Cron(args),
        TgCommand::Context(args) => Command::Context(args),
    };

    if let Err(e) = executor.execute(&session_id, &session_id, agent_output, cmd_output.clone(), cmd).await {
        cmd_output.send_error(&e);
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
                    RunExit::RecoverableFailed(ref msg)
                    | RunExit::CriticallyFailed(ref msg)
                    | RunExit::AutopilotStalled(ref msg) => {
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
