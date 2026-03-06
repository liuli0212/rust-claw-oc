use crate::core::{AgentOutput, RunExit};
use crate::session_manager::SessionManager;
use async_trait::async_trait;
use std::sync::Arc;
use teloxide::{
    net::Download,
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode},
    utils::command::BotCommands,
};
use tokio::sync::Mutex;

struct TelegramOutput {
    bot: Bot,
    chat_id: ChatId,
    text_buffer: Arc<Mutex<String>>,
}

impl TelegramOutput {
    fn new(bot: Bot, chat_id: ChatId) -> Self {
        Self {
            bot,
            chat_id,
            text_buffer: Arc::new(Mutex::new(String::new())),
        }
    }

    /// Strip ANSI escape codes (terminal color codes that leak from core.rs)
    fn strip_ansi(text: &str) -> String {
        let mut result = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                if chars.peek() == Some(&'[') {
                    chars.next(); // skip '['
                    while let Some(&nc) = chars.peek() {
                        chars.next();
                        if nc.is_ascii_alphabetic() {
                            break;
                        }
                    }
                    continue;
                }
            }
            result.push(c);
        }
        result
    }

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

    /// Summarize tool args for display (shared logic with CLI)
    fn summarize_tool_args(name: &str, args: &str) -> String {
        let args_val: serde_json::Value =
            serde_json::from_str(args).unwrap_or(serde_json::json!({}));
        let summary = match name {
            "read_file" | "write_file" | "patch_file" => args_val
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            "execute_bash" => args_val
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            "web_fetch" | "browser" => args_val
                .get("url")
                .or_else(|| args_val.get("target_url"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            _ => {
                let s = args_val.to_string();
                if s.len() > 80 {
                    format!("{}...", s.chars().take(80).collect::<String>())
                } else {
                    s
                }
            }
        };
        // Truncate to reasonable length for mobile
        if summary.len() > 100 {
            format!("{}...", summary.chars().take(100).collect::<String>())
        } else {
            summary
        }
    }

    async fn send_long_message(&self, text: &str, parse_mode: Option<ParseMode>) {
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
        let clean = Self::strip_ansi(text);
        let clean = clean.replace("<final>", "").replace("</final>", "");
        if !clean.is_empty() {
            self.text_buffer.lock().await.push_str(&clean);
        }
    }

    async fn on_thinking(&self, text: &str) {
        let clean = Self::strip_ansi(text);
        if !clean.is_empty() {
            let mut buf = self.text_buffer.lock().await;
            // Use Telegram blockquote format for visual distinction
            for line in clean.lines() {
                buf.push_str("> ");
                buf.push_str(line);
                buf.push('\n');
            }
        }
    }

    async fn on_tool_start(&self, name: &str, args: &str) {
        // Flush any buffered text first
        self.flush().await;

        let summary = Self::summarize_tool_args(name, args);
        let msg = format!(
            "🛠️ *{}*: `{}`",
            Self::escape_markdown_v2(name),
            Self::escape_markdown_v2(&summary)
        );

        // Create an Inline Button for cancellation
        let keyboard = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
            "🛑 停止任务",
            "cancel_task",
        )]]);

        if let Err(e) = self
            .bot
            .send_message(self.chat_id, msg)
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(keyboard)
            .await
        {
            tracing::error!("Failed to send Telegram tool start message: {}", e);
        }
    }

    async fn on_tool_end(&self, result: &str) {
        let mut ok = true;
        let mut display_name = "Result".to_string();
        let mut output_text = String::new();

        if let Ok(val) = serde_json::from_str::<serde_json::Value>(result) {
            if let Some(b) = val.get("ok").and_then(|v| v.as_bool()) {
                ok = b;
            }
            if let Some(n) = val.get("tool_name").and_then(|v| v.as_str()) {
                display_name = n.to_string();
            }
            // Extract clean output text from envelope
            if let Some(o) = val.get("output").and_then(|v| v.as_str()) {
                output_text = o.to_string();
            }
        }

        let status_emoji = if ok { "✅" } else { "❌" };
        let mut msg = format!(
            "{} *{}* completed",
            status_emoji,
            Self::escape_markdown_v2(&display_name)
        );

        // Show a brief snippet for failures or very short results
        if !ok {
            // Truncate error output to keep telegram message manageable
            let snippet: String = output_text.chars().take(200).collect();
            let snippet = snippet.lines().take(3).collect::<Vec<_>>().join("\n");
            if !snippet.is_empty() {
                msg.push_str(&format!("\n```\n{}\n```", Self::escape_pre_code(&snippet)));
            }
        }

        self.send_long_message(&msg, Some(ParseMode::MarkdownV2))
            .await;
    }

    async fn on_file(&self, path: &str) {
        self.flush().await;
        let path_buf = std::path::PathBuf::from(path);
        if !path_buf.exists() {
            tracing::error!("File not found for Telegram sending: {}", path);
            return;
        }

        let input_file = teloxide::types::InputFile::file(path_buf.clone());
        let ext = path_buf
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();

        let res = match ext.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "webp" => {
                self.bot.send_photo(self.chat_id, input_file).await
            }
            _ => self.bot.send_document(self.chat_id, input_file).await,
        };

        if let Err(e) = res {
            tracing::error!("Failed to send file to Telegram: {}", e);
        }
    }

    async fn on_error(&self, error: &str) {
        self.flush().await;
        let msg = format!("❌ *Error*: {}", Self::escape_markdown_v2(error));
        self.send_long_message(&msg, Some(ParseMode::MarkdownV2))
            .await;
    }

    async fn flush(&self) {
        let text = {
            let mut buf = self.text_buffer.lock().await;
            std::mem::take(&mut *buf)
        };
        if !text.is_empty() {
            // Send as plain text to avoid MarkdownV2 escaping issues with LLM output
            self.send_long_message(&text, None).await;
        }
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

    let handler = dptree::entry()
        .branch(Update::filter_callback_query().endpoint(handle_callback_query))
        .branch(
            Update::filter_message()
                .branch(
                    dptree::entry()
                        .filter_command::<Command>()
                        .endpoint(handle_command),
                )
                .branch(dptree::endpoint(handle_message)),
        );

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![session_manager])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

async fn handle_callback_query(
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
            let output = Arc::new(TelegramOutput::new(bot.clone(), chat_id));
            let agent = match session_manager
                .get_or_create_session(&session_id, output)
                .await
            {
                Ok(a) => a,
                Err(e) => {
                    bot.send_message(chat_id, format!("❌ Error: {}", e))
                        .await?;
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
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
        }
        Command::Model(args) => {
            let mut parts = args.split_whitespace();
            let provider = parts.next();
            let model = parts.next().map(|s| s.to_string());

            if let Some(p) = provider {
                match session_manager
                    .update_session_llm(&session_id, p, model)
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
            } else {
                bot.send_message(chat_id, "❌ Usage: /model <provider> [model_name]")
                    .await?;
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
                    if let Ok(_) = bot.download_file(&file.path, &mut dest).await {
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

        // Support emoji based stop — processed inline (no agent lock needed)
        if text == "🛑" || text == "🆘" || text.to_lowercase() == "stop" {
            session_manager.cancel_session(&session_id).await;
            bot.send_message(chat_id, "🛑 接收到紧急停止指令。").await?;
            return Ok(());
        }

        let output = Arc::new(TelegramOutput::new(bot.clone(), chat_id));

        let agent = match session_manager
            .get_or_create_session(&session_id, output)
            .await
        {
            Ok(a) => a,
            Err(e) => {
                bot.send_message(chat_id, format!("❌ Error: {}", e))
                    .await?;
                return Ok(());
            }
        };

        let text = text.to_string();
        let bot_clone = bot.clone();

        // Spawn agent execution in background so this handler returns immediately.
        // This allows teloxide's dispatcher to process subsequent updates
        // (stop button clicks, /cancel commands, stop text) while the agent runs.
        tokio::spawn(async move {
            // Send typing indicator in background
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

            let mut agent_guard = agent.lock().await;
            let result = agent_guard.step(text).await;
            drop(agent_guard); // Release lock before sending messages

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
