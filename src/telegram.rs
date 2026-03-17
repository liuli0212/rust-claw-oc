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
    #[command(description = "reset the current session.")]
    Reset,
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
}

pub async fn run_telegram_bot(token: String, session_manager: Arc<SessionManager>) {
    tracing::info!("Starting Telegram bot");
    let mut retry_count = 0;
    loop {
        // Use the compatible reqwest 0.11 client for teloxide
        let client = reqwest_teloxide::Client::builder()
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_else(|_| reqwest_teloxide::Client::new());

        let bot = Bot::with_client(token.clone(), client);
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
                let mut dispatcher = Dispatcher::builder(bot, handler)
                    .dependencies(dptree::deps![session_manager.clone()])
                    .enable_ctrlc_handler()
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
