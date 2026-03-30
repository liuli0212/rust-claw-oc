use crate::core::AgentOutput;
use crate::session_manager::SessionManager;
use async_trait::async_trait;
use chrono::Local;
use cron::Schedule;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub id: String,
    pub cron: String,
    pub goal: String,
    pub session_id: String,
    #[serde(default)]
    pub reply_to: String,
    pub enabled: bool,
    #[serde(default)]
    pub run_once: bool,
    pub last_run: Option<u64>,
}

pub struct Scheduler {
    tasks: Arc<RwLock<HashMap<String, ScheduledTask>>>,
    file_path: PathBuf,
    session_manager: Arc<SessionManager>,
}

struct CronOutput;

#[async_trait]
impl AgentOutput for CronOutput {
    async fn on_text(&self, text: &str) {
        tracing::info!("[Cron] {}", text);
    }
    async fn on_tool_start(&self, name: &str, args: &str) {
        tracing::info!("[Cron] Tool Start: {} with {}", name, args);
    }
    async fn on_tool_end(&self, result: &str) {
        tracing::info!("[Cron] Tool End: {}", result);
    }
    async fn on_error(&self, error: &str) {
        tracing::error!("[Cron] Error: {}", error);
    }
}

impl Scheduler {
    pub fn new(session_manager: Arc<SessionManager>, file_path: PathBuf) -> Self {
        let tasks = if file_path.exists() {
            let content = fs::read_to_string(&file_path).unwrap_or_default();
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            HashMap::new()
        };

        Self {
            tasks: Arc::new(RwLock::new(tasks)),
            file_path,
            session_manager,
        }
    }

    pub async fn reload(&self) {
        if self.file_path.exists() {
            if let Ok(content) = fs::read_to_string(&self.file_path) {
                if let Ok(parsed) = serde_json::from_str(&content) {
                    *self.tasks.write().await = parsed;
                }
            }
        }
    }

    pub async fn save(&self) -> Result<(), std::io::Error> {
        if let Some(parent) = self.file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tasks = self.tasks.read().await;
        let content = serde_json::to_string_pretty(&*tasks)?;
        fs::write(&self.file_path, content)?;
        Ok(())
    }

    pub async fn add_task(&self, task: ScheduledTask) -> Result<(), String> {
        // Validate cron
        Schedule::from_str(&task.cron).map_err(|e| format!("Invalid cron expression: {}", e))?;

        self.reload().await;
        let mut tasks = self.tasks.write().await;
        tasks.insert(task.id.clone(), task);
        drop(tasks);

        self.save().await.map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn remove_task(&self, id: &str) -> Result<(), String> {
        self.reload().await;
        let mut tasks = self.tasks.write().await;
        if tasks.remove(id).is_some() {
            drop(tasks);
            self.save().await.map_err(|e| e.to_string())?;
            Ok(())
        } else {
            Err("Task not found".to_string())
        }
    }

    pub async fn toggle_task(&self, id: &str, enabled: bool) -> Result<(), String> {
        self.reload().await;
        let mut tasks = self.tasks.write().await;
        if let Some(task) = tasks.get_mut(id) {
            task.enabled = enabled;
            drop(tasks);
            self.save().await.map_err(|e| e.to_string())?;
            Ok(())
        } else {
            Err("Task not found".to_string())
        }
    }

    pub async fn list_tasks(&self) -> Vec<ScheduledTask> {
        self.reload().await;
        let tasks = self.tasks.read().await;
        let mut list: Vec<ScheduledTask> = tasks.values().cloned().collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    pub async fn run_loop(self: Arc<Self>) {
        tracing::info!("Starting scheduler loop...");
        loop {
            self.reload().await;
            let now = Local::now();
            let mut tasks_to_run = Vec::new();

            {
                let mut tasks = self.tasks.write().await;
                for task in tasks.values_mut() {
                    if !task.enabled {
                        continue;
                    }

                    if let Ok(schedule) = Schedule::from_str(&task.cron) {
                        let window_start = now - chrono::Duration::seconds(60);
                        if let Some(next) = schedule.after(&window_start).next() {
                            // Check if it's time to run (within the last minute and not run yet)
                            let last_run_ts = task.last_run.unwrap_or(0);
                            let now_ts = now.timestamp() as u64;

                            tracing::debug!(
                                "[Scheduler] Task {}: next={}, now={}, last_run={}",
                                task.id,
                                next,
                                now,
                                last_run_ts
                            );

                            // If next run is in the past or now, and we haven't run in this minute
                            if next <= now && (now_ts / 60 > last_run_ts / 60) {
                                task.last_run = Some(now_ts);
                                tasks_to_run.push(task.clone());
                            }
                        }
                    }
                }
            }

            let mut tasks_to_remove = Vec::new();
            if !tasks_to_run.is_empty() {
                let mut tasks = self.tasks.write().await;
                for task in &tasks_to_run {
                    if task.run_once {
                        tasks.remove(&task.id);
                        tasks_to_remove.push(task.id.clone());
                    }
                }
                drop(tasks);
                let _ = self.save().await;
            }

            for task in tasks_to_run {
                let sm = self.session_manager.clone();
                tokio::spawn(async move {
                    tracing::info!("Executing scheduled task: {} ({})", task.id, task.goal);
                    let output = sm
                        .route_output(&task.reply_to)
                        .unwrap_or_else(|| Arc::new(CronOutput));
                    match sm
                        .get_or_create_session(&task.session_id, &task.reply_to, output.clone())
                        .await
                    {
                        Ok(agent_mutex) => {
                            {
                                let mut agent = agent_mutex.lock().await;
                                agent.update_output(output);
                            }
                            let mut agent = agent_mutex.lock().await;
                            let injected_goal = format!(
                                "[SYSTEM: SCHEDULED EVENT TRIGGERED]\nThis is a scheduled task that has just been triggered. Execute the goal immediately and output the result. DO NOT create new scheduled tasks or reminders for this.\n\nTask Goal: {}",
                                task.goal
                            );

                            if let Err(e) = agent.step(injected_goal).await {
                                tracing::error!("Scheduled task {} failed: {}", task.id, e);
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                "Failed to create session for scheduled task {}: {}",
                                task.id,
                                e
                            );
                        }
                    }
                });
            }

            sleep(Duration::from_secs(30)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_run_once_deletion() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("test_schedule.json");
        let sm = Arc::new(SessionManager::new(None, Vec::new()));
        let scheduler = Arc::new(Scheduler::new(sm, file_path));

        let task_id = "test_once".to_string();
        let task = ScheduledTask {
            id: task_id.clone(),
            cron: "* * * * * ? *".to_string(), // Run every second
            goal: "test".to_string(),
            session_id: "test".to_string(),
            reply_to: "test".to_string(),
            enabled: true,
            run_once: true,
            last_run: None,
        };

        scheduler.add_task(task).await.unwrap();

        // Initial state
        {
            let tasks = scheduler.tasks.read().await;
            assert!(tasks.contains_key(&task_id));
        }

        // Simulate run loop logic
        let now = Local::now();
        let mut tasks_to_run = Vec::new();
        {
            let mut tasks = scheduler.tasks.write().await;
            for task in tasks.values_mut() {
                if let Ok(schedule) = Schedule::from_str(&task.cron) {
                    if let Some(_next) =
                        schedule.after(&(now - chrono::Duration::seconds(1))).next()
                    {
                        tasks_to_run.push(task.clone());
                    }
                }
            }

            // Apply deletion logic
            for task in &tasks_to_run {
                if task.run_once {
                    tasks.remove(&task.id);
                }
            }
        }

        // Verify deletion
        {
            let tasks = scheduler.tasks.read().await;
            assert!(!tasks.contains_key(&task_id));
        }
    }

    #[tokio::test]
    async fn test_delay_cron_parsing() {
        use chrono::{TimeZone, Timelike};
        // Simulate a delay-generated cron
        // 27 40 8 21 3 ? 2026
        let cron_str = "27 40 8 21 3 ? 2026";
        let schedule = Schedule::from_str(cron_str).expect("Failed to parse cron");

        let now = Local.with_ymd_and_hms(2026, 3, 21, 8, 40, 30).unwrap();
        let window_start = now - chrono::Duration::seconds(60);

        let next = schedule
            .after(&window_start)
            .next()
            .expect("No next occurrence");
        println!("Now: {}", now);
        println!("Next: {}", next);

        assert!(next <= now);
        assert_eq!(next.second(), 27);
    }
}
