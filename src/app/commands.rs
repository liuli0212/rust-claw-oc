use crate::core::AgentOutput;
use crate::scheduler::Scheduler;
use crate::session_manager::SessionManager;
use crate::task_state::{TaskStateSnapshot, TaskStateStore};
use std::sync::Arc;

pub enum Command {
    Help,
    New,
    Cancel,
    CancelTask,
    Status,
    Session,
    Model(String),
    Cron(String),
    Context(String),
    Autopilot(String),
    Manual,
    Agent(String),
}

impl Command {
    pub fn parse(line: &str) -> Option<Self> {
        if !line.starts_with('/') {
            return None;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        let cmd = parts[0];
        let args = parts[1..].join(" ");
        match cmd {
            "/help" => Some(Command::Help),
            "/new" => Some(Command::New),
            "/cancel" => Some(Command::Cancel),
            "/cancel_task" => Some(Command::CancelTask),
            "/status" => Some(Command::Status),
            "/session" => Some(Command::Session),
            "/model" => Some(Command::Model(args)),
            "/cron" => Some(Command::Cron(args)),
            "/context" => Some(Command::Context(args)),
            "/autopilot" => Some(Command::Autopilot(args)),
            "/manual" => Some(Command::Manual),
            _ => Some(Command::Agent(line.to_string())),
        }
    }
}

pub struct StatusData {
    pub provider: String,
    pub model: String,
    pub tokens: usize,
    pub max_tokens: usize,
    pub active_plan: Option<TaskStateSnapshot>,
}

pub trait CommandOutput: Send + Sync {
    fn send_text(&self, text: &str);
    fn send_error(&self, error: &str);
    fn send_success(&self, message: &str);
    fn send_status(&self, data: StatusData);
    fn send_session_list(&self, sessions: Vec<(String, u64, usize)>);
    fn send_cron_list(&self, tasks: Vec<crate::scheduler::ScheduledTask>);
    fn send_context_audit(&self, details: String);
    fn send_context_diff(&self, diff: Option<String>);
    fn send_context_inspect(&self, result: String);
    fn send_context_dump(&self, path: String);
    fn send_context_compact(&self, result: Result<(), String>);
}

pub struct CommandExecutor {
    session_manager: Arc<SessionManager>,
}

impl CommandExecutor {
    pub fn new(session_manager: Arc<SessionManager>) -> Self {
        Self { session_manager }
    }

    pub async fn execute(
        &self,
        session_id: &str,
        reply_to: &str,
        agent_output: Arc<dyn AgentOutput>,
        cmd_output: Arc<dyn CommandOutput>,
        cmd: Command,
    ) -> Result<(), String> {
        match cmd {
            Command::Help => {
                // Help is usually platform-specific
                Ok(())
            }
            Command::New => {
                self.session_manager.reset_session(session_id).await;
                let ts = TaskStateStore::new(session_id);
                let _ = ts.clear();
                cmd_output.send_success("Session and task plan cleared. Starting fresh.");
                Ok(())
            }
            Command::Cancel => {
                self.session_manager.cancel_session(session_id).await;
                cmd_output.send_success("Request cancelled.");
                Ok(())
            }
            Command::CancelTask => {
                self.session_manager.cancel_session(session_id).await;
                let ts = TaskStateStore::new(session_id);
                let _ = ts.clear();
                cmd_output.send_success("Task cancelled and plan cleared.");
                Ok(())
            }
            Command::Status => {
                let agent = self
                    .session_manager
                    .get_or_create_session(session_id, reply_to, agent_output)
                    .await?;
                let agent_guard = agent.lock().await;
                let (provider, model, tokens, max_tokens) = agent_guard.get_status();

                let ts = TaskStateStore::new(session_id);
                let active_plan = if ts.has_active_plan() {
                    ts.load().ok()
                } else {
                    None
                };

                cmd_output.send_status(StatusData {
                    provider,
                    model,
                    tokens,
                    max_tokens,
                    active_plan,
                });
                Ok(())
            }
            Command::Session => {
                let sessions = self.session_manager.list_sessions();
                cmd_output.send_session_list(sessions);
                Ok(())
            }
            Command::Model(args) => {
                let parts: Vec<&str> = args.split_whitespace().collect();
                if parts.is_empty() {
                    return Err("Usage: /model <provider> [model_name]".to_string());
                }
                let provider = parts[0];
                let model = parts.get(1).map(|s| s.to_string());
                match self
                    .session_manager
                    .update_session_llm(session_id, provider, model)
                    .await
                {
                    Ok(msg) => {
                        cmd_output.send_success(&msg);
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
            Command::Cron(args) => {
                let parts: Vec<&str> = args.split_whitespace().collect();
                let action = parts.first().copied().unwrap_or("list");
                let scheduler = Scheduler::new(self.session_manager.clone());

                match action {
                    "list" => {
                        let tasks = scheduler.list_tasks().await;
                        cmd_output.send_cron_list(tasks);
                    }
                    "remove" => {
                        if let Some(id) = parts.get(1) {
                            match scheduler.remove_task(id).await {
                                Ok(_) => {
                                    cmd_output.send_success(&format!("Task '{}' removed.", id))
                                }
                                Err(e) => return Err(e.to_string()),
                            }
                        } else {
                            return Err("Usage: /cron remove <id>".to_string());
                        }
                    }
                    "toggle" => {
                        if let (Some(id), Some(state)) = (parts.get(1), parts.get(2)) {
                            let enabled = match *state {
                                "on" | "true" | "enable" => true,
                                "off" | "false" | "disable" => false,
                                _ => return Err("Usage: /cron toggle <id> <on|off>".to_string()),
                            };
                            match scheduler.toggle_task(id, enabled).await {
                                Ok(_) => cmd_output.send_success(&format!(
                                    "Task '{}' is now {}.",
                                    id,
                                    if enabled { "enabled" } else { "disabled" }
                                )),
                                Err(e) => return Err(e.to_string()),
                            }
                        } else {
                            return Err("Usage: /cron toggle <id> <on|off>".to_string());
                        }
                    }
                    _ => return Err("Unknown action. Use: list, remove, toggle".to_string()),
                }
                Ok(())
            }
            Command::Context(args) => {
                let agent = self
                    .session_manager
                    .get_or_create_session(session_id, reply_to, agent_output)
                    .await?;
                let mut agent_guard = agent.lock().await;
                let parts: Vec<&str> = args.split_whitespace().collect();
                let subcommand = parts.first().copied().unwrap_or("");

                match subcommand {
                    "audit" => {
                        cmd_output.send_context_audit(agent_guard.get_context_details());
                    }
                    "diff" => {
                        if let Some(diff) = agent_guard.diff_snapshot() {
                            cmd_output.send_context_diff(Some(agent_guard.format_diff(&diff)));
                        } else {
                            cmd_output.send_context_diff(None);
                        }
                    }
                    "inspect" => {
                        let section = parts.get(1).copied().unwrap_or("");
                        let arg = parts.get(2).copied();
                        cmd_output.send_context_inspect(agent_guard.inspect_context(section, arg));
                    }
                    "dump" => {
                        let (payload, sys, report) = agent_guard.build_llm_payload();
                        let dump_data = serde_json::json!({
                            "system_prompt": sys,
                            "messages": payload,
                            "tools": agent_guard.get_tools_metadata(),
                            "report": report,
                        });
                        if let Ok(json_str) = serde_json::to_string_pretty(&dump_data) {
                            let filename = if session_id.starts_with("telegram:") {
                                format!(
                                    "debug_context_tg_{}.json",
                                    session_id.strip_prefix("telegram:").unwrap()
                                )
                            } else {
                                "debug_context.json".to_string()
                            };
                            if std::fs::write(&filename, json_str).is_ok() {
                                cmd_output.send_context_dump(filename);
                            } else {
                                return Err("Failed to write dump file".to_string());
                            }
                        }
                    }
                    "compact" => {
                        let res = agent_guard
                            .maybe_compact_history(true)
                            .await
                            .map_err(|e| e.to_string());
                        cmd_output.send_context_compact(res);
                    }
                    _ => {
                        // Default context view handled by output
                        cmd_output.send_context_audit("".to_string()); // Trigger default view
                    }
                }
                Ok(())
            }
            Command::Autopilot(_goal) => {
                let agent = self
                    .session_manager
                    .get_or_create_session(session_id, reply_to, agent_output)
                    .await?;
                let mut agent_guard = agent.lock().await;
                agent_guard.enable_autopilot();
                cmd_output.send_success("Autopilot mode enabled.");
                // Note: The caller should handle running the step if goal is not empty
                Ok(())
            }
            Command::Manual => {
                let agent = self
                    .session_manager
                    .get_or_create_session(session_id, reply_to, agent_output)
                    .await?;
                let mut agent_guard = agent.lock().await;
                agent_guard.is_autopilot = false;
                cmd_output.send_success("Autopilot mode disabled. Switched to manual mode.");
                Ok(())
            }
            Command::Agent(msg) => {
                let agent = self
                    .session_manager
                    .get_or_create_session(session_id, reply_to, agent_output)
                    .await?;
                let mut agent_guard = agent.lock().await;
                let _ = agent_guard.step(msg).await.map_err(|e| e.to_string())?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_parse_passthrough() {
        // Unknown command starting with / should be captured as Command::Agent
        // to allow it to be passed through to the agent's skill runtime.
        let line = "/system_audit --check-all";
        let cmd = Command::parse(line).expect("Should parse unknown slash command");
        if let Command::Agent(msg) = cmd {
            assert_eq!(msg, line);
        } else {
            panic!("Expected Command::Agent for unknown slash command");
        }

        // Known command should still work as before
        let cmd_new = Command::parse("/new").expect("Should parse /new");
        assert!(matches!(cmd_new, Command::New));

        // Non-command (not starting with /) should remain None
        let cmd_text = Command::parse("hi agent");
        assert!(cmd_text.is_none());
    }
}
