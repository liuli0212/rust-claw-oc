use crate::app::commands::{Command, CommandExecutor, CommandOutput, StatusData};
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
    text_buffer: Arc<Mutex<String>>,
    streaming_message_id: Arc<Mutex<Option<serenity::model::id::MessageId>>>,
    last_update: Arc<Mutex<std::time::Instant>>,
}

impl DiscordOutput {
    fn new(ctx: Context, channel_id: serenity::model::id::ChannelId) -> Self {
        Self {
            ctx,
            channel_id,
            text_buffer: Arc::new(Mutex::new(String::new())),
            streaming_message_id: Arc::new(Mutex::new(None)),
            last_update: Arc::new(Mutex::new(std::time::Instant::now())),
        }
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

        // Throttle updates to every 2 seconds for Discord
        if !force && now.duration_since(last) < std::time::Duration::from_secs(2) {
            return;
        }

        let mut streaming_id_guard = self.streaming_message_id.lock().await;
        if let Some(msg_id) = *streaming_id_guard {
            // Edit the message
            let builder = serenity::builder::EditMessage::new().content(text);
            let _ = self
                .channel_id
                .edit_message(&self.ctx.http, msg_id, builder)
                .await;
        } else {
            // Start a new message
            if let Ok(msg) = self.channel_id.say(&self.ctx.http, &text).await {
                *streaming_id_guard = Some(msg.id);
            }
        }

        let mut last_guard = self.last_update.lock().await;
        *last_guard = now;
    }

    async fn flush_internal(
        &self,
        streaming_id_guard: &mut tokio::sync::MutexGuard<
            '_,
            Option<serenity::model::id::MessageId>,
        >,
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
            let builder = serenity::builder::EditMessage::new().content(text);
            let _ = self
                .channel_id
                .edit_message(&self.ctx.http, msg_id, builder)
                .await;
        } else {
            let _ = self.channel_id.say(&self.ctx.http, &text).await;
        }
        **streaming_id_guard = None;
    }
}

#[async_trait]
impl AgentOutput for DiscordOutput {
    fn clear_waiting(&self) {}

    async fn on_waiting(&self, _message: &str) {
        let _ = self.channel_id.broadcast_typing(&self.ctx.http).await;
    }

    async fn on_text(&self, text: &str) {
        if !text.is_empty() {
            self.text_buffer.lock().await.push_str(text);
            self.maybe_update_live_message(false).await;
        }
    }

    async fn on_thinking(&self, text: &str) {
        if !text.is_empty() {
            let mut buf = self.text_buffer.lock().await;
            if buf.is_empty() || buf.ends_with('\n') {
                buf.push_str("> 🧠 ");
            }
            let indented = text.replace('\n', "\n> ");
            buf.push_str(&indented);
            drop(buf);
            self.maybe_update_live_message(false).await;
        }
    }

    async fn on_tool_start(&self, name: &str, args: &str) {
        self.flush().await;
        let msg = format!(
            "🛠️ **Tool Call**: `{}`\nArgs:\n```{}\n```",
            name,
            Self::truncate_to_three_lines(args)
        );
        let _ = self.channel_id.say(&self.ctx.http, msg).await;
    }

    async fn on_tool_end(&self, result: &str) {
        let mut ok = true;
        let mut output_text = result.to_string();

        if let Ok(val) = serde_json::from_str::<serde_json::Value>(result) {
            if let Some(b) = val.get("ok").and_then(|v| v.as_bool()) {
                ok = b;
            }
            if let Some(o) = val.get("output").and_then(|v| v.as_str()) {
                output_text = o.to_string();
            }
        }

        let display_result = if output_text.len() > 1800 {
            format!("{}... (truncated)", &output_text[..1800])
        } else {
            output_text
        };
        let status_emoji = if ok { "✅" } else { "❌" };
        let msg = format!("{} **Tool Result**:\n```\n{}\n```", status_emoji, display_result);
        let _ = self.channel_id.say(&self.ctx.http, msg).await;
    }

    async fn on_error(&self, error: &str) {
        self.flush().await;
        let msg = format!("❌ **Error**: {}", error);
        let _ = self.channel_id.say(&self.ctx.http, msg).await;
    }

    async fn flush(&self) {
        let mut streaming_id_guard = self.streaming_message_id.lock().await;
        self.flush_internal(&mut streaming_id_guard).await;
    }
}

struct DiscordCommandOutput {
    ctx: Context,
    channel_id: serenity::model::id::ChannelId,
}

impl CommandOutput for DiscordCommandOutput {
    fn send_text(&self, text: &str) {
        let http = self.ctx.http.clone();
        let channel_id = self.channel_id;
        let text = text.to_string();
        tokio::spawn(async move {
            let _ = channel_id.say(&http, text).await;
        });
    }

    fn send_error(&self, error: &str) {
        let http = self.ctx.http.clone();
        let channel_id = self.channel_id;
        let msg = format!("❌ **Error**: {}", error);
        tokio::spawn(async move {
            let _ = channel_id.say(&http, msg).await;
        });
    }

    fn send_success(&self, message: &str) {
        let http = self.ctx.http.clone();
        let channel_id = self.channel_id;
        let msg = format!("✅ {}", message);
        tokio::spawn(async move {
            let _ = channel_id.say(&http, msg).await;
        });
    }

    fn send_status(&self, data: StatusData) {
        let http = self.ctx.http.clone();
        let channel_id = self.channel_id;
        tokio::spawn(async move {
            let mut status_msg = "📊 **Bot Status**\n".to_string();
            status_msg.push_str(&format!("*Provider*: {}\n*Model*: {}\n*Context*: {}/{} tokens\n", data.provider, data.model, data.tokens, data.max_tokens));

            if let Some(state) = data.active_plan {
                status_msg.push_str(&format!(
                    "🎯 **Active Task**: {}\n",
                    state.goal.unwrap_or_else(|| "Unknown".to_string())
                ));
                for (i, step) in state.plan_steps.iter().enumerate() {
                    let icon = match step.status.as_str() {
                        "completed" => "✅",
                        "in_progress" => "⏳",
                        _ => "⬜",
                    };
                    status_msg.push_str(&format!(
                        "  [{}] {} {}\n",
                        i, icon, step.step
                    ));
                }
                status_msg.push_str(
                    "\n💡 You can say \"continue\" to proceed, or use `/cancel_task` to abort.",
                );
            } else {
                status_msg.push_str("✅ No active task.");
            }
            let _ = channel_id.say(&http, status_msg).await;
        });
    }

    fn send_session_list(&self, sessions: Vec<(String, u64, usize)>) {
        let http = self.ctx.http.clone();
        let channel_id = self.channel_id;
        tokio::spawn(async move {
            let mut msg = "📝 **Active/Recent Sessions**\n━━━━━━━━━━━━━━━━━━━━━\n".to_string();
            if sessions.is_empty() {
                msg.push_str("(No sessions found)");
            } else {
                for (id, updated, turns) in sessions {
                    msg.push_str(&format!("• `{}` (Turns: {}, Updated: {})\n", id, turns, updated));
                }
            }
            let _ = channel_id.say(&http, msg).await;
        });
    }

    fn send_cron_list(&self, tasks: Vec<crate::scheduler::ScheduledTask>) {
        let http = self.ctx.http.clone();
        let channel_id = self.channel_id;
        tokio::spawn(async move {
            if tasks.is_empty() {
                let _ = channel_id.say(&http, "⚪ No scheduled tasks found.").await;
            } else {
                let mut msg = "📅 **Scheduled Tasks**\\n━━━━━━━━━━━━━━━━━━━━━\\n".to_string();
                for task in tasks {
                    let status = if task.enabled { "✅" } else { "❌" };
                    msg.push_str(&format!(
                        "*ID*: `{}` {}\\n*Cron*: `{}`\\n*Goal*: {}\\n\\n",
                        task.id, status, task.cron, task.goal
                    ));
                }
                let _ = channel_id.say(&http, msg).await;
            }
        });
    }

    fn send_context_audit(&self, details: String) {
        self.send_text(&details);
    }

    fn send_context_diff(&self, diff: Option<String>) {
        if let Some(diff) = diff {
            self.send_text(&diff);
        } else {
            self.send_text("ℹ️ No changes since last snapshot.");
        }
    }

    fn send_context_inspect(&self, result: String) {
        self.send_text(&result);
    }

    fn send_context_dump(&self, path: String) {
        self.send_success(&format!("Context dumped locally to {}", path));
    }

    fn send_context_compact(&self, result: Result<(), String>) {
        match result {
            Ok(_) => self.send_success("Compaction finished."),
            Err(e) => self.send_error(&format!("Compaction failed: {}", e)),
        }
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
        let output = Arc::new(DiscordOutput::new(ctx.clone(), msg.channel_id));
        let executor = CommandExecutor::new(self.session_manager.clone());
        let cmd_output = Arc::new(DiscordCommandOutput { ctx: ctx.clone(), channel_id: msg.channel_id });

        let content = msg.content.clone();
        let channel_id = msg.channel_id;
        let http = ctx.http.clone();

        if let Some(cmd) = Command::parse(&content) {
            if let Err(e) = executor.execute(&session_id, &session_id, output.clone(), cmd_output.clone(), cmd).await {
                cmd_output.send_error(&e);
            }
            return;
        }

        let agent = match self
            .session_manager
            .get_or_create_session(&session_id, &session_id, output.clone())
            .await
        {
            Ok(a) => a,
            Err(e) => {
                let _ = msg
                    .channel_id
                    .say(&ctx.http, format!("❌ Error: {}", e))
                    .await;
                return;
            }
        };

        // Spawn agent execution in background so EventHandler returns immediately
        tokio::spawn(async move {
            // Try to acquire the agent lock without blocking indefinitely.
            // If the previous task is still running, notify the user instead of silently queuing.
            let mut agent_guard =
                match tokio::time::timeout(std::time::Duration::from_secs(3), agent.lock()).await {
                    Ok(guard) => guard,
                    Err(_) => {
                        let _ = channel_id
                        .say(
                            &http,
                            "⏳ Previous task is still running. Please wait or cancel it first.",
                        )
                        .await;
                        return;
                    }
                };

            let _ = output.on_waiting("Processing...").await;

            // Before stepping, flush any previous buffered un-sent text and update the output
            agent_guard.flush_output().await;
            agent_guard.update_output(output.clone());

            let result = agent_guard.step(content).await;
            drop(agent_guard);

            match result {
                Ok(exit) => match exit {
                    crate::core::RunExit::RecoverableFailed(ref e)
                    | crate::core::RunExit::CriticallyFailed(ref e)
                    | crate::core::RunExit::AutopilotStalled(ref e) => {
                        let _ = channel_id
                            .say(
                                &http,
                                format!("⚠️ Run stopped: {}\\nReason: {}", exit.label(), e),
                            )
                            .await;
                    }
                    crate::core::RunExit::StoppedByUser => {
                        let _ = channel_id.say(&http, "✅ Task stopped by user.").await;
                    }
                    _ => {}
                },
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
