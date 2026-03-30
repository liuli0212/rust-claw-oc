use crate::core::{AgentOutput, OutputRouter, SilentOutputWrapper};
use crate::session_manager::SessionManager;
use std::sync::Arc;
use std::time::Duration;
use teloxide::{prelude::*, utils::command::BotCommands};

mod handlers;
mod output;

use handlers::{handle_callback_query, handle_command, handle_message};

#[derive(BotCommands, Clone)]
#[command(
    rename_rule = "lowercase",
    description = "These commands are supported:"
)]
enum Command {
    #[command(description = "display this text.")]
    Help,
    #[command(description = "start a fresh session.")]
    New,
    #[command(description = "check if bot is alive.")]
    Ping,
    #[command(description = "cancel the current running task.")]
    Cancel,
    #[command(description = "show session status.")]
    Status,
    #[command(description = "show detailed session diagnostics.")]
    Session,
    #[command(description = "switch LLM model: /model <provider> [model_name]")]
    Model(String),
    #[command(description = "manage scheduled tasks: /cron <list|remove|toggle> [id] [on|off]")]
    Cron(String),
    #[command(description = "inspect context: /context <audit|diff|inspect|dump|compact>")]
    Context(String),
    #[command(description = "enable autopilot mode: /autopilot [goal]")]
    Autopilot(String),
    #[command(description = "switch to manual mode")]
    Manual,
}

pub struct TelegramOutputRouter {
    pub bot: Bot,
}

pub fn parse_telegram_reply_to(reply_to: &str) -> Option<i64> {
    reply_to
        .strip_prefix("tg_")
        .or_else(|| reply_to.strip_prefix("telegram:"))
        .and_then(|chat_id_str| chat_id_str.parse::<i64>().ok())
}

impl OutputRouter for TelegramOutputRouter {
    fn try_route(&self, reply_to: &str) -> Option<Arc<dyn AgentOutput>> {
        tracing::debug!("TelegramOutputRouter checking reply_to: '{}'", reply_to);
        if let Some(id) = parse_telegram_reply_to(reply_to) {
            let base_output = Arc::new(output::TelegramOutput::new(
                self.bot.clone(),
                teloxide::types::ChatId(id),
            ));
            return Some(Arc::new(SilentOutputWrapper { inner: base_output }));
        }
        None
    }
}

pub async fn run_telegram_bot(token: String, session_manager: Arc<SessionManager>) {
    tracing::info!("Starting Telegram bot");

    // Use the compatible reqwest 0.11 client for teloxide
    let client = reqwest_teloxide::Client::builder()
        .timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_else(|_| reqwest_teloxide::Client::new());

    let bot = Bot::with_client(token.clone(), client);

    session_manager.add_output_router(Arc::new(TelegramOutputRouter { bot: bot.clone() }));

    let mut retry_count = 0;
    loop {
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

        tracing::info!("Checking Telegram connection (Attempt {})", retry_count + 1);
        match bot.get_me().await {
            Ok(me) => {
                tracing::info!("Telegram bot @{} connected successfully.", me.username());
                let mut dispatcher = Dispatcher::builder(bot.clone(), handler.clone())
                    .dependencies(dptree::deps![session_manager.clone()])
                    .build();

                // Use spawn to isolate potential panics from the main process
                match tokio::spawn(async move { dispatcher.dispatch().await }).await {
                    Ok(_) => {
                        tracing::warn!("Telegram dispatcher exited normally.");
                        break;
                    }
                    Err(e) => {
                        tracing::error!("Telegram dispatcher crashed: {}. Retrying...", e);
                    }
                }
            }
            Err(e) => {
                tracing::error!("Telegram connection failed: {}. Retrying in 30s...", e);
            }
        }
        retry_count += 1;
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}
