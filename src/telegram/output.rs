use crate::core::AgentOutput;
use async_trait::async_trait;
use std::sync::Arc;
use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode},
};
use tokio::sync::Mutex;

pub(super) struct TelegramOutput {
    bot: Bot,
    chat_id: ChatId,
    text_buffer: Arc<Mutex<String>>,
    active_plan_message_id: Arc<Mutex<Option<teloxide::types::MessageId>>>,
    streaming_message_id: Arc<Mutex<Option<teloxide::types::MessageId>>>,
    last_update: Arc<Mutex<std::time::Instant>>,
}

impl TelegramOutput {
    pub(super) fn new(bot: Bot, chat_id: ChatId) -> Self {
        Self {
            bot,
            chat_id,
            text_buffer: Arc::new(Mutex::new(String::new())),
            active_plan_message_id: Arc::new(Mutex::new(None)),
            streaming_message_id: Arc::new(Mutex::new(None)),
            last_update: Arc::new(Mutex::new(std::time::Instant::now())),
        }
    }

    pub(super) fn escape_markdown_v2(text: &str) -> String {
        let to_escape = "_*[]()~`>#+-=|{}.!\\";
        let mut escaped = String::with_capacity(text.len());
        for c in text.chars() {
            if to_escape.contains(c) {
                escaped.push('\\');
            }
            escaped.push(c);
        }
        escaped
    }

    fn strip_ansi(text: &str) -> String {
        let mut result = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if nc.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            result.push(c);
        }
        result
    }

    fn escape_pre_code(text: &str) -> String {
        text.replace('\\', "\\\\").replace('`', "\\`")
    }

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
                if parse_mode.is_some() {
                    let _ = self.bot.send_message(self.chat_id, chunk).await;
                }
            }
            start = end;
        }
    }

    async fn maybe_update_live_message(&self, force: bool) {
        let (text, now, last) = {
            let buf = self.text_buffer.lock().await;
            let now = std::time::Instant::now();
            let last = self.last_update.lock().await;
            (buf.clone(), now, *last)
        };

        if text.is_empty() {
            return;
        }

        if !force && now.duration_since(last) < std::time::Duration::from_secs(2) {
            return;
        }

        let mut streaming_id_guard = self.streaming_message_id.lock().await;
        if let Some(msg_id) = *streaming_id_guard {
            if let Err(e) = self
                .bot
                .edit_message_text(self.chat_id, msg_id, &text)
                .await
            {
                let err_str = e.to_string();
                if err_str.contains("message is not modified") {
                    tracing::trace!("Telegram live update: message not modified");
                } else {
                    tracing::error!("Failed to edit live Telegram message: {}", e);
                    *streaming_id_guard = None;
                }

                if text.len() > 3500 {
                    self.flush_internal(&mut streaming_id_guard).await;
                }
            }
        } else if let Ok(msg) = self.bot.send_message(self.chat_id, &text).await {
            *streaming_id_guard = Some(msg.id);
        }

        let mut last_guard = self.last_update.lock().await;
        *last_guard = now;
    }

    async fn flush_internal(
        &self,
        streaming_id_guard: &mut tokio::sync::MutexGuard<'_, Option<teloxide::types::MessageId>>,
    ) {
        let text = {
            let mut buf = self.text_buffer.lock().await;
            std::mem::take(&mut *buf)
        };
        if text.is_empty() {
            **streaming_id_guard = None;
            return;
        }

        if let Some(msg_id) = **streaming_id_guard {
            tracing::debug!(
                "Flushing Telegram stream via edit: msg_id={:?}, len={}",
                msg_id,
                text.len()
            );
            if let Err(e) = self
                .bot
                .edit_message_text(self.chat_id, msg_id, &text)
                .await
            {
                let err_str = e.to_string();
                if !err_str.contains("message is not modified") {
                    tracing::error!(
                        "Failed to flush Telegram message via edit: {}. Falling back to new message.",
                        e
                    );
                    self.send_long_message(&text, None).await;
                }
            }
        } else {
            tracing::debug!(
                "Flushing Telegram stream via new message: len={}",
                text.len()
            );
            self.send_long_message(&text, None).await;
        }
        **streaming_id_guard = None;
    }
}

#[async_trait]
impl AgentOutput for TelegramOutput {
    fn clear_waiting(&self) {}

    async fn on_waiting(&self, _message: &str) {
        let _ = self
            .bot
            .send_chat_action(self.chat_id, teloxide::types::ChatAction::Typing)
            .await;
    }

    async fn on_text(&self, text: &str) {
        let clean = Self::strip_ansi(text);
        let clean = clean.replace("<final>", "").replace("</final>", "");
        if !clean.is_empty() {
            self.text_buffer.lock().await.push_str(&clean);
            self.maybe_update_live_message(false).await;
        }
    }

    async fn on_thinking(&self, text: &str) {
        let clean = Self::strip_ansi(text);
        if !clean.is_empty() {
            let mut buf = self.text_buffer.lock().await;
            if buf.is_empty() || buf.ends_with('\n') {
                buf.push_str("> 🧠 ");
            }
            let indented = clean.replace('\n', "\n> ");
            buf.push_str(&indented);
            drop(buf);
            self.maybe_update_live_message(false).await;
        }
    }

    async fn on_tool_start(&self, name: &str, args: &str) {
        self.flush().await;

        let summary = Self::summarize_tool_args(name, args);
        let msg = format!(
            "🛠️ *{}*: `{}`",
            Self::escape_markdown_v2(name),
            Self::escape_markdown_v2(&summary)
        );

        let keyboard = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
            "🛑 停止任务",
            "cancel_task",
        )]]);

        if let Err(e) = self
            .bot
            .send_message(self.chat_id, &msg)
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(keyboard.clone())
            .await
        {
            tracing::error!("Failed to send Telegram tool start message: {}", e);
            let plain_msg = format!("🛠️ {}: {}", name, summary);
            let _ = self
                .bot
                .send_message(self.chat_id, plain_msg)
                .reply_markup(keyboard)
                .await;
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

        if !ok {
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
        let mut streaming_id_guard = self.streaming_message_id.lock().await;
        self.flush_internal(&mut streaming_id_guard).await;
    }

    async fn on_plan_update(&self, state: &crate::task_state::TaskStateSnapshot) {
        if state.plan_steps.is_empty() {
            return;
        }

        let mut lines = Vec::new();
        if let Some(goal) = &state.goal {
            lines.push(format!(
                "🎯 *Objective*: {}",
                Self::escape_markdown_v2(goal)
            ));
            lines.push(String::new());
        }

        lines.push(format!(
            "*Plan Overview* \\({}\\):",
            Self::escape_markdown_v2(&state.status)
        ));

        for (i, step) in state.plan_steps.iter().enumerate() {
            let icon = match step.status.as_str() {
                "completed" => "✅",
                "in_progress" => "🔄",
                _ => "⏳",
            };

            let mut line = format!(
                "{} {}\\. {}",
                icon,
                i + 1,
                Self::escape_markdown_v2(&step.step)
            );
            if let Some(note) = &step.note {
                if !note.is_empty() {
                    line.push_str(&format!(" \\- _{}_", Self::escape_markdown_v2(note)));
                }
            }
            lines.push(line);
        }

        let text = lines.join("\n");
        let mut active_msg_id = self.active_plan_message_id.lock().await;

        if let Some(msg_id) = *active_msg_id {
            let res = self
                .bot
                .edit_message_text(self.chat_id, msg_id, &text)
                .parse_mode(ParseMode::MarkdownV2)
                .await;

            if res.is_err() {
                *active_msg_id = None;
            } else {
                return;
            }
        }

        if let Ok(msg) = self
            .bot
            .send_message(self.chat_id, &text)
            .parse_mode(ParseMode::MarkdownV2)
            .await
        {
            *active_msg_id = Some(msg.id);
        } else {
            let plain_text = text.replace('\\', "");
            if let Ok(msg) = self.bot.send_message(self.chat_id, plain_text).await {
                *active_msg_id = Some(msg.id);
            }
        }
    }

    async fn on_task_finish(&self, summary: &str) {
        self.flush().await;

        let lines = [
            "🎉 *Task Completed*".to_string(),
            String::new(),
            Self::escape_markdown_v2(summary),
        ];

        let text = lines.join("\n");
        self.send_long_message(&text, Some(ParseMode::MarkdownV2))
            .await;

        let mut active_msg_id = self.active_plan_message_id.lock().await;
        *active_msg_id = None;
    }
}
