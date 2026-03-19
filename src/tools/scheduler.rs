use crate::scheduler::{ScheduledTask, Scheduler};
use crate::tools::protocol::{Tool, ToolError};
use async_trait::async_trait;
use chrono::{Datelike, Timelike};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub struct ManageScheduleTool {
    pub scheduler: Arc<Scheduler>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ManageScheduleArgs {
    /// Action to perform: "add", "remove", "list", "toggle"
    pub action: String,
    /// Unique ID for the task (required for "add", "remove", "toggle")
    pub id: Option<String>,
    /// Cron expression (e.g., "0 23 * * *") (required for "add")
    pub cron: Option<String>,
    /// The goal/instruction for the agent to execute (required for "add")
    pub goal: Option<String>,
    /// Session ID to use for the task (optional, defaults to "cron_default")
    pub session_id: Option<String>,
    /// Delay from now (e.g., "5m", "1h", "10s"). If used, task is run once.
    pub delay: Option<String>,
    /// For "toggle": true to enable, false to disable
    pub enabled: Option<bool>,
}

#[async_trait]
impl Tool for ManageScheduleTool {
    fn name(&self) -> String {
        "manage_schedule".to_string()
    }

    fn description(&self) -> String {
        "Manage scheduled tasks and reminders. Actions: add, remove, list, toggle. Supports relative delays like '5m' or '1h' for one-time reminders.".to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        crate::tools::protocol::clean_schema(
            serde_json::to_value(schemars::schema_for!(ManageScheduleArgs)).unwrap(),
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &crate::tools::ToolContext,
    ) -> Result<String, crate::tools::ToolError> {
        let parsed: ManageScheduleArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        match parsed.action.as_str() {
            "add" => {
                let id = parsed.id.ok_or_else(|| {
                    ToolError::InvalidArguments("id is required for add".to_string())
                })?;
                let goal = parsed.goal.ok_or_else(|| {
                    ToolError::InvalidArguments("goal is required for add".to_string())
                })?;
                let session_id = parsed.session_id.unwrap_or_else(|| format!("cron_{}", id));

                let mut run_once = false;
                let cron = if let Some(delay_str) = parsed.delay {
                    run_once = true;
                    // Simple parsing for "10s", "5m", "1h"
                    let seconds = if delay_str.ends_with("s") {
                        delay_str.trim_end_matches('s').parse::<u64>().unwrap_or(0)
                    } else if delay_str.ends_with("m") {
                        delay_str.trim_end_matches('m').parse::<u64>().unwrap_or(0) * 60
                    } else if delay_str.ends_with("h") {
                        delay_str.trim_end_matches('h').parse::<u64>().unwrap_or(0) * 3600
                    } else {
                        return Err(ToolError::InvalidArguments(
                            "Format delay as '10s', '5m', or '1h'".into(),
                        ));
                    };

                    let target_time =
                        chrono::Local::now() + chrono::Duration::seconds(seconds as i64);
                    // Generate a 7-field cron: sec min hour day month dow year
                    // Note: year is important for run_once to avoid any collision
                    format!(
                        "{} {} {} {} {} ? {}",
                        target_time.second(),
                        target_time.minute(),
                        target_time.hour(),
                        target_time.day(),
                        target_time.month(),
                        target_time.year()
                    )
                } else {
                    parsed.cron.ok_or_else(|| {
                        ToolError::InvalidArguments("cron or delay is required for add".to_string())
                    })?
                };

                // If the task is created from CLI, try to forward notifications to Telegram
                // if TELEGRAM_CHAT_ID is configured in the environment.
                let mut reply_to = _ctx.reply_to.clone();
                if reply_to == "cli" {
                    if let Ok(chat_id) = std::env::var("TELEGRAM_CHAT_ID") {
                        reply_to = format!("telegram:{}", chat_id);
                    }
                }

                let task = ScheduledTask {
                    id: id.clone(),
                    cron,
                    goal,
                    session_id,
                    reply_to,
                    enabled: true,
                    run_once,
                    last_run: None,
                };

                self.scheduler
                    .add_task(task)
                    .await
                    .map_err(ToolError::ExecutionFailed)?;

                Ok(format!(
                    "Task '{}' added successfully. Run once: {}",
                    id, run_once
                ))
            }
            "remove" => {
                let id = parsed.id.ok_or_else(|| {
                    ToolError::InvalidArguments("id is required for remove".to_string())
                })?;
                self.scheduler
                    .remove_task(&id)
                    .await
                    .map_err(ToolError::ExecutionFailed)?;
                Ok(format!("Task '{}' removed successfully.", id))
            }
            "toggle" => {
                let id = parsed.id.ok_or_else(|| {
                    ToolError::InvalidArguments("id is required for toggle".to_string())
                })?;
                let enabled = parsed.enabled.ok_or_else(|| {
                    ToolError::InvalidArguments("enabled is required for toggle".to_string())
                })?;
                self.scheduler
                    .toggle_task(&id, enabled)
                    .await
                    .map_err(ToolError::ExecutionFailed)?;
                Ok(format!(
                    "Task '{}' is now {}.",
                    id,
                    if enabled { "enabled" } else { "disabled" }
                ))
            }
            "list" => {
                let tasks = self.scheduler.list_tasks().await;
                Ok(serde_json::to_string_pretty(&tasks).unwrap())
            }
            _ => Err(ToolError::InvalidArguments(format!(
                "Unknown action: {}",
                parsed.action
            ))),
        }
    }
}
