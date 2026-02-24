use crate::core::AgentOutput;
use crate::session_manager::SessionManager;
use async_trait::async_trait;
use std::sync::Arc;
use teloxide::prelude::*;

struct TelegramOutput {
    bot: Bot,
    chat_id: ChatId,
}

#[async_trait]
impl AgentOutput for TelegramOutput {
    async fn on_text(&self, text: &str) {
        let _ = self.bot.send_message(self.chat_id, text).await;
    }

    async fn on_tool_start(&self, name: &str, args: &str) {
        let msg = format!("🛠️ Tool Call: {}\nArgs: {}", name, args);
        let _ = self.bot.send_message(self.chat_id, msg).await;
    }

    async fn on_tool_end(&self, result: &str) {
        let display_result = if result.len() > 3000 {
            format!("{}... (truncated)", &result[..3000])
        } else {
            result.to_string()
        };
        let msg = format!("✅ Tool Result:\n{}", display_result);
        let _ = self.bot.send_message(self.chat_id, msg).await;
    }

    async fn on_error(&self, error: &str) {
        let msg = format!("❌ Error: {}", error);
        let _ = self.bot.send_message(self.chat_id, msg).await;
    }
}

pub async fn run_telegram_bot(token: String, session_manager: Arc<SessionManager>) {
    tracing::info!("Starting Telegram bot");
    let bot = Bot::new(token);

    teloxide::repl(bot, move |bot: Bot, msg: Message| {
        let session_manager = session_manager.clone();
        async move {
            if let Some(text) = msg.text() {
                let chat_id = msg.chat.id;
                let session_id = format!("telegram:{}", chat_id);

                let output = Arc::new(TelegramOutput {
                    bot: bot.clone(),
                    chat_id,
                });

                let agent = session_manager
                    .get_or_create_session(&session_id, output)
                    .await;

                let mut agent_guard = agent.lock().await;
                match agent_guard.step(text.to_string()).await {
                    Ok(exit) => {
                        if matches!(exit, crate::core::RunExit::RecoverableFailed { .. }) {
                            let _ = bot
                                .send_message(chat_id, format!("Run stopped: {}", exit.label()))
                                .await;
                        }
                    }
                    Err(e) => {
                        let _ = bot.send_message(chat_id, format!("Error: {}", e)).await;
                    }
                }
            }
            Ok(())
        }
    })
    .await;
}
