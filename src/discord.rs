use crate::core::AgentOutput;
use crate::session_manager::SessionManager;
use serenity::async_trait;
use serenity::model::channel::Message;
use serenity::model::gateway::Ready;
use serenity::prelude::*;
use std::sync::Arc;

struct DiscordOutput {
    ctx: Context,
    channel_id: serenity::model::id::ChannelId,
}

impl DiscordOutput {
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
}

#[async_trait]
impl AgentOutput for DiscordOutput {
    async fn on_text(&self, text: &str) {
        let _ = self.channel_id.say(&self.ctx.http, text).await;
    }

    async fn on_tool_start(&self, name: &str, args: &str) {
        let msg = format!(
            "🛠️ **Tool Call**: `{}`\nArgs:\n```{}\n```",
            name,
            Self::truncate_to_three_lines(args)
        );
        let _ = self.channel_id.say(&self.ctx.http, msg).await;
    }

    async fn on_tool_end(&self, result: &str) {
        let display_result = if result.len() > 1800 {
            format!("{}... (truncated)", &result[..1800])
        } else {
            result.to_string()
        };
        let msg = format!("✅ **Tool Result**:\n```\n{}\n```", display_result);
        let _ = self.channel_id.say(&self.ctx.http, msg).await;
    }

    async fn on_error(&self, error: &str) {
        let msg = format!("❌ **Error**: {}", error);
        let _ = self.channel_id.say(&self.ctx.http, msg).await;
    }
}

struct Handler {
    session_manager: Arc<SessionManager>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }

        let session_id = format!("discord:{}", msg.channel_id);
        let output = Arc::new(DiscordOutput {
            ctx: ctx.clone(),
            channel_id: msg.channel_id,
        });

        let agent = match self
            .session_manager
            .get_or_create_session(&session_id, output)
            .await {
                Ok(a) => a,
                Err(e) => {
                    let _ = msg.channel_id.say(&ctx.http, format!("❌ Error: {}", e)).await;
                    return;
                }
            };

        let content = msg.content.clone();
        let channel_id = msg.channel_id;
        let http = ctx.http.clone();

        // Spawn agent execution in background so EventHandler returns immediately
        tokio::spawn(async move {
            let mut agent_guard = agent.lock().await;
            let result = agent_guard.step(content).await;
            drop(agent_guard);

            match result {
                Ok(exit) => {
                    match exit {
                        crate::core::RunExit::AgentTurnLimitReached => {
                            let _ = channel_id.say(&http, "⚠️ [Turn Limit Reached]").await;
                        }
                        crate::core::RunExit::RecoverableFailed(ref e) | crate::core::RunExit::CriticallyFailed(ref e) => {
                            let _ = channel_id.say(&http, format!("⚠️ Run stopped: {}\nReason: {}", exit.label(), e)).await;
                        }
                        crate::core::RunExit::StoppedByUser => {
                            let _ = channel_id.say(&http, "✅ Task stopped by user.").await;
                        }
                        _ => {}
                    }
                }
                Err(e) => {
                    let _ = channel_id.say(&http, format!("Error: {}", e)).await;
                }
            }
        });
    }

    async fn ready(&self, _: Context, ready: Ready) {
        tracing::info!("Discord Bot {} is connected", ready.user.name);
    }
}

pub async fn run_discord_bot(token: String, session_manager: Arc<SessionManager>) {
    tracing::info!("Starting Discord bot");
    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT;
    let mut client = Client::builder(&token, intents)
        .event_handler(Handler { session_manager })
        .await
        .expect("Err creating Discord client");

    if let Err(why) = client.start().await {
        tracing::error!("Discord client error: {:?}", why);
    }
}
