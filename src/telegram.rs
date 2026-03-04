use crate::core::{AgentOutput, RunExit};
use crate::session_manager::SessionManager;
use async_trait::async_trait;
use std::sync::Arc;
use teloxide::{prelude::*, utils::command::BotCommands};

struct TelegramOutput {
    bot: Bot,
    chat_id: ChatId,
}

impl TelegramOutput {
    fn escape_markdown_v2(text: &str) -> String {
        let to_escape = r"_*[]()~`>#+-=|{}.!";
        let mut escaped = String::with_capacity(text.len());
        for c in text.chars() {
            if to_escape.contains(c) {
                escaped.push('\\');
            }
            escaped.push(c);
        }
        escaped
    }

    fn escape_pre_code(text: &str) -> String {
        text.replace('\\', "\\\\").replace('`', "\\`")
    }

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

    async fn send_long_message(&self, text: &str, parse_mode: Option<teloxide::types::ParseMode>) {
        if text.is_empty() {
            return;
        }

        const MAX_LEN: usize = 4000;
        let mut start = 0;
        while start < text.len() {
            let mut end = (start + MAX_LEN).min(text.len());
            
            // Try not to break in the middle of a multi-byte character
            while end > start && !text.is_char_boundary(end) {
                end -= 1;
            }

            let chunk = &text[start..end];
            let mut req = self.bot.send_message(self.chat_id, chunk);
            if let Some(mode) = parse_mode {
                req = req.parse_mode(mode);
            }
            
            if let Err(e) = req.await {
                tracing::error!("Failed to send Telegram message: {}", e);
                // Fallback to plain text if markdown fails
                if parse_mode.is_some() {
                    let _ = self.bot.send_message(self.chat_id, chunk).await;
                }
            }
            start = end;
        }
    }
}

#[async_trait]
impl AgentOutput for TelegramOutput {
    async fn on_text(&self, text: &str) {
        // We don't use Markdown for streaming text to avoid partial tag issues
        self.send_long_message(text, None).await;
    }

    async fn on_tool_start(&self, name: &str, args: &str) {
        let msg = format!(
            "🛠️ *Tool Call*: `{}`\n*Args*:\n```\n{}\n```",
            Self::escape_markdown_v2(name),
            Self::escape_pre_code(&Self::truncate_to_three_lines(args))
        );
        self.send_long_message(&msg, Some(teloxide::types::ParseMode::MarkdownV2)).await;
    }

    async fn on_tool_end(&self, result: &str) {
        let display_result = if result.len() > 3000 {
            format!("{}... (truncated)", &result[..3000])
        } else {
            result.to_string()
        };
        let msg = format!(
            "✅ *Tool Result*:\n```\n{}\n```",
            Self::escape_pre_code(&display_result)
        );
        self.send_long_message(&msg, Some(teloxide::types::ParseMode::MarkdownV2)).await;
    }

    async fn on_error(&self, error: &str) {
        let msg = format!("❌ *Error*: {}", Self::escape_markdown_v2(error));
        self.send_long_message(&msg, Some(teloxide::types::ParseMode::MarkdownV2)).await;
    }
}

#[derive(BotCommands, Clone)]
#[command(
    rename_rule = "lowercase",
    description = "These commands are supported:"
)]
enum Command {
    #[command(description = "display this text.")]
    Help,
    #[command(description = "reset the current session.")]
    Reset,
    #[command(description = "cancel the current running task.")]
    Cancel,
    #[command(description = "show session status.")]
    Status,
    #[command(description = "switch LLM model: /model <provider> [model_name]")]
    Model(String),
}

pub async fn run_telegram_bot(token: String, session_manager: Arc<SessionManager>) {
    tracing::info!("Starting Telegram bot");
    let bot = Bot::new(token);

    let handler = Update::filter_message().branch(
        dptree::entry()
            .filter_command::<Command>()
            .endpoint(handle_command),
    ).branch(
        dptree::endpoint(handle_message)
    );

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![session_manager])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

async fn handle_command(
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
        Command::Cancel => {
            session_manager.cancel_session(&session_id).await;
            bot.send_message(chat_id, "🛑 Task cancellation requested.")
                .await?;
        }
        Command::Status => {
            let output = Arc::new(TelegramOutput {
                bot: bot.clone(),
                chat_id,
            });
            let agent = match session_manager
                .get_or_create_session(&session_id, output)
                .await
            {
                Ok(a) => a,
                Err(e) => {
                    bot.send_message(chat_id, format!("❌ Error: {}", e)).await?;
                    return Ok(());
                }
            };
            let agent_guard = agent.lock().await;
            let (provider, model, tokens, max_tokens) = agent_guard.get_status();
            let status = format!(
                "🤖 *Status*\n*Provider*: {}\n*Model*: {}\n*Context*: {} / {} tokens",
                TelegramOutput::escape_markdown_v2(&provider),
                TelegramOutput::escape_markdown_v2(&model),
                tokens,
                max_tokens
            );
            bot.send_message(chat_id, status)
                .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                .await?;
        }
        Command::Model(args) => {
            let mut parts = args.split_whitespace();
            let provider = parts.next();
            let model = parts.next().map(|s| s.to_string());
            
            if let Some(p) = provider {
                match session_manager.update_session_llm(&session_id, p, model).await {
                    Ok(msg) => {
                        bot.send_message(chat_id, format!("✅ {}", msg)).await?;
                    }
                    Err(e) => {
                        bot.send_message(chat_id, format!("❌ Error: {}", e)).await?;
                    }
                }
            } else {
                bot.send_message(chat_id, "❌ Usage: /model <provider> [model_name]").await?;
            }
        }
    }
    Ok(())
}

async fn handle_message(
    bot: Bot,
    msg: Message,
    session_manager: Arc<SessionManager>,
) -> ResponseResult<()> {
    if let Some(text) = msg.text() {
        let chat_id = msg.chat.id;
        let session_id = format!("telegram:{}", chat_id);

        let output = Arc::new(TelegramOutput {
            bot: bot.clone(),
            chat_id,
        });

        let agent = match session_manager
            .get_or_create_session(&session_id, output)
            .await
        {
            Ok(a) => a,
            Err(e) => {
                bot.send_message(chat_id, format!("❌ Error: {}", e)).await?;
                return Ok(());
            }
        };

        // Send typing indicator in background
        let bot_clone = bot.clone();
        let typing_done = Arc::new(tokio::sync::Notify::new());
        let typing_done_clone = typing_done.clone();
        
        tokio::spawn(async move {
            loop {
                let _ = bot_clone.send_chat_action(chat_id, teloxide::types::ChatAction::Typing).await;
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {},
                    _ = typing_done_clone.notified() => break,
                }
            }
        });

        let mut agent_guard = agent.lock().await;
        let result = agent_guard.step(text.to_string()).await;
        
        typing_done.notify_one();

        match result {
            Ok(exit) => {
                match exit {
                    RunExit::AgentTurnLimitReached => {
                        bot.send_message(chat_id, "⚠️ [Turn Limit Reached] The agent reached the maximum allowed consecutive actions. Please type 'continue' if you want it to proceed.").await?;
                    }
                    RunExit::RecoverableFailed(ref msg) | RunExit::CriticallyFailed(ref msg) => {
                        bot.send_message(chat_id, format!("⚠️ Run stopped: {}\nReason: {}", exit.label(), msg)).await?;
                    }
                    _ => {}
                }
            }
            Err(e) => {
                bot.send_message(chat_id, format!("❌ Error: {}", e)).await?;
            }
        }
    }
    Ok(())
}
