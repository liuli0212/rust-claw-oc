use crate::core::{AgentLoop, AgentOutput, OutputRouter};
use crate::llm_client::LlmClient;
use crate::tools::Tool;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex as AsyncMutex;

type SessionEntryMap = AsyncMutex<
    HashMap<
        String,
        (
            Arc<AsyncMutex<AgentLoop>>,
            std::sync::Arc<tokio::sync::Notify>,
            std::sync::Arc<std::sync::atomic::AtomicBool>,
        ),
    >,
>;

pub struct SessionManager {
    scheduler: std::sync::RwLock<Option<Arc<crate::scheduler::Scheduler>>>,

    llm: Arc<RwLock<Option<Arc<dyn LlmClient>>>>,
    tools: RwLock<Vec<Arc<dyn Tool>>>,
    code_mode_format: crate::code_mode::description::CodeModeFormat,
    subagent_runtime: Arc<RwLock<Option<crate::subagent_runtime::SubagentRuntime>>>,
    routers: RwLock<Vec<Arc<dyn OutputRouter>>>,
    sessions: SessionEntryMap,
    registry: crate::session::repository::SessionRegistryStore,
}

impl SessionManager {
    pub fn new(llm: Option<Arc<dyn LlmClient>>, tools: Vec<Arc<dyn Tool>>) -> Self {
        Self::new_with_code_mode_format(
            llm,
            tools,
            crate::code_mode::description::CodeModeFormat::default(),
        )
    }

    pub fn new_with_code_mode_format(
        llm: Option<Arc<dyn LlmClient>>,
        tools: Vec<Arc<dyn Tool>>,
        code_mode_format: crate::code_mode::description::CodeModeFormat,
    ) -> Self {
        let runtime = llm.as_ref().map(|llm| {
            crate::subagent_runtime::SubagentRuntime::new(llm.clone(), tools.clone(), 3)
        });
        Self {
            llm: Arc::new(RwLock::new(llm)),
            tools: RwLock::new(tools),
            code_mode_format,
            subagent_runtime: Arc::new(RwLock::new(runtime)),
            routers: RwLock::new(Vec::new()),
            sessions: AsyncMutex::new(HashMap::new()),
            scheduler: std::sync::RwLock::new(None),
            registry: crate::session::repository::SessionRegistryStore::new(
                std::path::PathBuf::from("rusty_claw"),
            ),
        }
    }

    pub fn set_scheduler(&self, scheduler: Arc<crate::scheduler::Scheduler>) {
        *self.scheduler.write().unwrap() = Some(scheduler);
    }

    pub fn scheduler(&self) -> Option<Arc<crate::scheduler::Scheduler>> {
        self.scheduler.read().unwrap().clone()
    }

    pub fn add_output_router(&self, router: Arc<dyn OutputRouter>) {
        let mut routers = self.routers.write().unwrap();
        routers.push(router);
    }

    pub fn route_output(&self, reply_to: &str) -> Option<Arc<dyn AgentOutput>> {
        let routers = self.routers.read().unwrap();
        tracing::debug!(
            "Routing output for reply_to: {}, routers count: {}",
            reply_to,
            routers.len()
        );
        for router in routers.iter() {
            if let Some(output) = router.try_route(reply_to) {
                tracing::debug!("Found router for reply_to: {}", reply_to);
                return Some(output);
            }
        }
        tracing::debug!("No router found for reply_to: {}", reply_to);
        None
    }

    pub fn add_tool(&self, tool: Arc<dyn Tool>) {
        let mut tools = self.tools.write().unwrap();
        tools.push(tool);
        let llm = self.llm.read().unwrap().clone();
        let runtime =
            llm.map(|llm| crate::subagent_runtime::SubagentRuntime::new(llm, tools.clone(), 3));
        *self.subagent_runtime.write().unwrap() = runtime;
    }

    pub async fn reset_session(&self, session_id: &str) {
        let mut sessions = self.sessions.lock().await;

        // Remove from memory
        sessions.remove(session_id);

        self.registry.remove_session_artifacts(session_id);
    }

    pub async fn cancel_session(&self, session_id: &str) {
        let session_entry = {
            let sessions = self.sessions.lock().await;
            sessions.get(session_id).map(|(agent, notify, cancelled)| {
                (agent.clone(), notify.clone(), cancelled.clone())
            })
        };

        if let Some((agent, notify, cancelled)) = session_entry {
            cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
            notify.notify_waiters();
            tracing::info!("Cancel requested for session: {}", session_id);

            // If the agent is not currently inside step(), this cancel came from
            // the idle prompt. In that state no foreground dispatch loop exists
            // to observe cancel_token, so clean up any background code-mode cell
            // directly. If step() is running, avoid blocking on its mutex; the
            // running loop will handle the same cancel notification.
            if let Ok(agent_guard) = agent.try_lock() {
                let aborted = agent_guard
                    .abort_active_code_mode("Session cancelled by user.")
                    .await;
                if aborted {
                    tracing::info!(
                        "Aborted active code-mode cell for idle session: {}",
                        session_id
                    );
                }
            }
        }
    }

    pub async fn get_or_create_session(
        &self,
        session_id: &str,
        reply_to: &str,
        output: Arc<dyn AgentOutput>,
    ) -> Result<Arc<AsyncMutex<AgentLoop>>, String> {
        let existing = {
            let sessions = self.sessions.lock().await;
            sessions.get(session_id).map(|(a, _, _)| a.clone())
        };

        if let Some(agent_mutex) = existing {
            self.registry.touch_session(session_id, None, None);
            return Ok(agent_mutex.clone());
        }

        let transcript_path = self.registry.transcript_path(session_id);

        let llm = {
            let llm_guard = self.llm.read().unwrap();
            llm_guard
                .as_ref()
                .ok_or_else(|| {
                    "No LLM provider configured. Use /model <provider> to set one.".to_string()
                })?
                .clone()
        };
        let tools = self.tools.read().unwrap().clone();
        let subagent_runtime = {
            let mut runtime_guard = self.subagent_runtime.write().unwrap();
            if runtime_guard.is_none() {
                *runtime_guard = Some(crate::subagent_runtime::SubagentRuntime::new(
                    llm.clone(),
                    tools.clone(),
                    3,
                ));
            }
            runtime_guard
                .as_ref()
                .expect("subagent runtime should be initialized")
                .clone()
        };
        let agent = crate::session::factory::build_agent_session(
            session_id,
            reply_to,
            llm,
            tools,
            subagent_runtime,
            transcript_path.clone(),
            output,
            self.code_mode_format,
        )?;
        let loaded_turns = agent.lock().await.context.dialogue_history.len();
        self.registry
            .touch_session(session_id, Some(&transcript_path), Some(loaded_turns));
        let token = agent.lock().await.cancel_token.clone();
        let cancelled = agent.lock().await.cancelled.clone();

        let mut sessions = self.sessions.lock().await;
        sessions.insert(session_id.to_string(), (agent.clone(), token, cancelled));
        Ok(agent)
    }

    pub async fn update_session_llm(
        &self,
        session_id: &str,
        provider: &str,
        model: Option<String>,
    ) -> Result<String, String> {
        let config = crate::config::AppConfig::load();
        // We use the factory function from llm_client
        match crate::llm_client::create_llm_client(provider, model.clone(), None, &config) {
            Ok(new_llm) => {
                // Update global default for new sessions
                {
                    let mut llm_guard = self.llm.write().unwrap();
                    *llm_guard = Some(new_llm.clone());
                }
                {
                    let tools = self.tools.read().unwrap().clone();
                    let mut runtime_guard = self.subagent_runtime.write().unwrap();
                    *runtime_guard = Some(crate::subagent_runtime::SubagentRuntime::new(
                        new_llm.clone(),
                        tools,
                        3,
                    ));
                }

                let agent_mutex = {
                    let sessions = self.sessions.lock().await;
                    sessions.get(session_id).map(|(a, _, _)| a.clone())
                };
                if let Some(agent_mutex) = agent_mutex {
                    let mut agent = agent_mutex.lock().await;
                    agent.update_llm(new_llm);
                    Ok(format!(
                        "Updated session '{}' and global default to provider '{}' model '{:?}'",
                        session_id, provider, model
                    ))
                } else {
                    // Even if session doesn't exist, we updated the global default.
                    Ok(format!("Updated global default to provider '{}' model '{:?}'. (Session '{}' not yet active)", provider, model, session_id))
                }
            }
            Err(e) => Err(e),
        }
    }

    #[cfg(feature = "acp")]
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.read().unwrap().clone()
    }

    pub fn list_sessions(&self) -> Vec<(String, u64, usize)> {
        self.registry.list_sessions()
    }

    pub fn subagent_runtime(&self) -> crate::subagent_runtime::SubagentRuntime {
        self.subagent_runtime
            .read()
            .unwrap()
            .as_ref()
            .expect("subagent runtime should be initialized")
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use serde_json::{json, Value};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    use crate::context::{FunctionCall, Message};
    use crate::core::RunExit;
    use crate::llm_client::{LlmError, StreamEvent};
    use crate::tools::protocol::{StructuredToolOutput, ToolContext, ToolExecutionEnvelope};
    use crate::tools::ToolError;

    #[derive(Default)]
    struct CaptureOutput {
        tool_results: std::sync::Mutex<Vec<String>>,
        errors: std::sync::Mutex<Vec<String>>,
        finish_summaries: std::sync::Mutex<Vec<String>>,
    }

    impl CaptureOutput {
        fn tool_results(&self) -> Vec<String> {
            self.tool_results.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl AgentOutput for CaptureOutput {
        async fn on_text(&self, _text: &str) {}

        async fn on_tool_start(&self, _name: &str, _args: &str) {}

        async fn on_tool_end(&self, result: &str) {
            self.tool_results.lock().unwrap().push(result.to_string());
        }

        async fn on_error(&self, error: &str) {
            self.errors.lock().unwrap().push(error.to_string());
        }

        async fn on_task_finish(&self, summary: &str) {
            self.finish_summaries
                .lock()
                .unwrap()
                .push(summary.to_string());
        }
    }

    struct MockReadTool;

    #[async_trait]
    impl Tool for MockReadTool {
        fn name(&self) -> String {
            "read_file".to_string()
        }

        fn description(&self) -> String {
            "Mock read-only tool".to_string()
        }

        fn parameters_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                }
            })
        }

        fn has_side_effects(&self) -> bool {
            false
        }

        async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
            StructuredToolOutput::new(
                "read_file",
                true,
                "mock read".to_string(),
                Some(0),
                None,
                false,
            )
            .to_json_string()
        }
    }

    struct AsyncSubagentScenarioLlm;

    static SESSION_MANAGER_TEST_MUTEX: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

    #[async_trait]
    impl LlmClient for AsyncSubagentScenarioLlm {
        fn model_name(&self) -> &str {
            "async-subagent-scenario"
        }

        fn provider_name(&self) -> &str {
            "test"
        }

        fn capabilities(&self) -> crate::llm_client::LlmCapabilities {
            crate::llm_client::LlmCapabilities {
                function_tools: true,
                custom_tools: false,
                parallel_tool_calls: true,
                supports_code_mode: true,
            }
        }

        async fn stream(
            &self,
            messages: Vec<Message>,
            system_instruction: Option<Message>,
            tools: Vec<Arc<dyn Tool>>,
        ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
            let tool_names: Vec<String> = tools.iter().map(|tool| tool.name()).collect();
            let user_text = latest_user_text(&messages);
            let system_text = system_instruction
                .and_then(|message| {
                    message
                        .parts
                        .into_iter()
                        .find_map(|part| part.text.map(|text| text.to_string()))
                })
                .unwrap_or_default();

            if tool_names.iter().any(|name| name == "subagent") {
                parent_stream(user_text, &messages, system_text)
            } else {
                subagent_stream(user_text)
            }
        }
    }

    fn latest_user_text(messages: &[Message]) -> String {
        messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .and_then(|message| {
                message
                    .parts
                    .iter()
                    .find_map(|part| part.text.as_ref().map(ToString::to_string))
            })
            .unwrap_or_default()
    }

    fn extract_job_ids_from_messages(messages: &[Message]) -> Vec<String> {
        let mut job_ids = Vec::new();
        for message in messages {
            for part in &message.parts {
                let Some(response) = &part.function_response else {
                    continue;
                };
                let Some(result_json) = response.response.get("result").and_then(Value::as_str)
                else {
                    continue;
                };
                let Some(envelope) = ToolExecutionEnvelope::from_json_str(result_json) else {
                    continue;
                };
                if envelope.result.tool_name != "subagent" {
                    continue;
                }
                let Ok(payload) = serde_json::from_str::<Value>(&envelope.result.output) else {
                    continue;
                };
                if payload.get("status").and_then(Value::as_str) == Some("spawned") {
                    if let Some(job_id) = payload.get("job_id").and_then(Value::as_str) {
                        job_ids.push(job_id.to_string());
                    }
                }
            }
        }
        job_ids
    }

    fn count_tool_responses(messages: &[Message], expected_status_or_key: &str) -> usize {
        messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| part.function_response.as_ref())
            .filter_map(|response| response.response.get("result").and_then(Value::as_str))
            .filter_map(ToolExecutionEnvelope::from_json_str)
            .filter(|envelope| envelope.result.tool_name == "subagent")
            .filter(|envelope| {
                if let Ok(payload) = serde_json::from_str::<Value>(&envelope.result.output) {
                    if expected_status_or_key == "jobs" {
                        payload.get("jobs").is_some()
                    } else if expected_status_or_key == "debug" {
                        payload.get("debug").is_some()
                    } else if expected_status_or_key == "spawned" {
                        payload.get("status").and_then(Value::as_str) == Some("spawned")
                    } else if expected_status_or_key == "cancelling" {
                        payload.get("status").and_then(Value::as_str) == Some("cancelling")
                    } else {
                        false
                    }
                } else {
                    false
                }
            })
            .count()
    }

    fn parent_stream(
        user_text: String,
        messages: &[Message],
        system_text: String,
    ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
        let (tx, rx) = mpsc::channel(16);
        let job_ids = extract_job_ids_from_messages(messages);
        let spawned_count = count_tool_responses(messages, "spawned");
        let collected_count = count_tool_responses(messages, "debug");
        let listed_count = count_tool_responses(messages, "jobs");
        let cancelled_count = count_tool_responses(messages, "cancelling");

        let events = if user_text.contains("spawn one background job") {
            if spawned_count == 0 {
                vec![make_tool_call(
                    "subagent",
                    json!({
                        "action": "run",
                        "run_in_background": true,
                        "goal": "inspect alpha module",
                        "input_summary": "Inspect alpha module and report findings.",
                        "allowed_tools": ["read_file"],
                        "timeout_sec": 20,
                        "max_steps": 4
                    }),
                    "parent_spawn_1",
                )]
            } else {
                vec![make_tool_call(
                    "finish_task",
                    json!({ "summary": "spawned background job" }),
                    "parent_finish_1",
                )]
            }
        } else if user_text.contains("collect one background job") {
            if collected_count < job_ids.len() {
                let next_job_id = &job_ids[collected_count];
                vec![make_tool_call(
                    "subagent",
                    json!({ "action": "status", "job_id": next_job_id }),
                    &format!("parent_get_{}", collected_count + 1),
                )]
            } else {
                vec![make_tool_call(
                    "finish_task",
                    json!({ "summary": "collected background job" }),
                    "parent_finish_2",
                )]
            }
        } else if user_text.contains("spawn failing background job") {
            if spawned_count == 0 {
                vec![make_tool_call(
                    "subagent",
                    json!({
                        "action": "run",
                        "run_in_background": true,
                        "goal": "force fail module",
                        "input_summary": "This subagent should fail immediately.",
                        "allowed_tools": ["read_file"],
                        "timeout_sec": 20,
                        "max_steps": 4
                    }),
                    "mixed_spawn_1",
                )]
            } else {
                vec![make_tool_call(
                    "finish_task",
                    json!({ "summary": "spawned failing job" }),
                    "mixed_finish_1",
                )]
            }
        } else if user_text.contains("collect failing background job") {
            if collected_count < job_ids.len() {
                let next_job_id = &job_ids[collected_count];
                vec![make_tool_call(
                    "subagent",
                    json!({ "action": "status", "job_id": next_job_id }),
                    &format!("mixed_get_{}", collected_count + 1),
                )]
            } else {
                vec![make_tool_call(
                    "finish_task",
                    json!({ "summary": "collected failing job" }),
                    "mixed_finish_2",
                )]
            }
        } else if user_text.contains("spawn two background jobs") {
            if spawned_count == 0 {
                vec![make_tool_call(
                    "subagent",
                    json!({
                        "action": "run",
                        "run_in_background": true,
                        "goal": "inspect alpha module",
                        "input_summary": "Inspect alpha module and report findings.",
                        "allowed_tools": ["read_file"],
                        "timeout_sec": 20,
                        "max_steps": 4
                    }),
                    "parent_spawn_1",
                )]
            } else if spawned_count == 1 {
                vec![make_tool_call(
                    "subagent",
                    json!({
                        "action": "run",
                        "run_in_background": true,
                        "goal": "inspect beta module",
                        "input_summary": "Inspect beta module and report findings.",
                        "allowed_tools": ["read_file"],
                        "timeout_sec": 20,
                        "max_steps": 4
                    }),
                    "parent_spawn_2",
                )]
            } else {
                vec![make_tool_call(
                    "finish_task",
                    json!({ "summary": "spawned background jobs" }),
                    "parent_finish_1",
                )]
            }
        } else if user_text.contains("collect two background jobs") {
            if listed_count == 0 {
                vec![make_tool_call(
                    "subagent",
                    json!({ "action": "list" }),
                    "parent_list_jobs",
                )]
            } else if collected_count < job_ids.len() {
                let next_job_id = &job_ids[collected_count];
                vec![make_tool_call(
                    "subagent",
                    json!({ "action": "status", "job_id": next_job_id }),
                    &format!("parent_get_{}", collected_count + 1),
                )]
            } else {
                vec![make_tool_call(
                    "finish_task",
                    json!({ "summary": "collected background jobs" }),
                    "parent_finish_2",
                )]
            }
        } else if user_text.contains("spawn success and fail jobs") {
            if spawned_count == 0 {
                vec![make_tool_call(
                    "subagent",
                    json!({
                        "action": "run",
                        "run_in_background": true,
                        "goal": "inspect success module",
                        "input_summary": "Return a successful summary.",
                        "allowed_tools": ["read_file"],
                        "timeout_sec": 20,
                        "max_steps": 4
                    }),
                    "mixed_spawn_1",
                )]
            } else if spawned_count == 1 {
                vec![make_tool_call(
                    "subagent",
                    json!({
                        "action": "run",
                        "run_in_background": true,
                        "goal": "force fail module",
                        "input_summary": "This subagent should fail immediately.",
                        "allowed_tools": ["read_file"],
                        "timeout_sec": 20,
                        "max_steps": 4
                    }),
                    "mixed_spawn_2",
                )]
            } else {
                vec![make_tool_call(
                    "finish_task",
                    json!({ "summary": "spawned mixed jobs" }),
                    "mixed_finish_1",
                )]
            }
        } else if user_text.contains("collect mixed job results") {
            if collected_count < job_ids.len() {
                let next_job_id = &job_ids[collected_count];
                vec![make_tool_call(
                    "subagent",
                    json!({ "action": "status", "job_id": next_job_id }),
                    &format!("mixed_get_{}", collected_count + 1),
                )]
            } else {
                vec![make_tool_call(
                    "finish_task",
                    json!({ "summary": "collected mixed jobs" }),
                    "mixed_finish_2",
                )]
            }
        } else if user_text.contains("spawn hanging job") {
            if spawned_count == 0 {
                vec![make_tool_call(
                    "subagent",
                    json!({
                        "action": "run",
                        "run_in_background": true,
                        "goal": "hang forever",
                        "input_summary": "This subagent should hang until cancelled.",
                        "allowed_tools": ["read_file"],
                        "timeout_sec": 30,
                        "max_steps": 4
                    }),
                    "hang_spawn",
                )]
            } else {
                vec![make_tool_call(
                    "finish_task",
                    json!({ "summary": "spawned hanging job" }),
                    "hang_finish_1",
                )]
            }
        } else if user_text.contains("cancel hanging job") {
            let first_job = job_ids.first().cloned().unwrap_or_default();
            if cancelled_count == 0 {
                vec![make_tool_call(
                    "subagent",
                    json!({ "action": "cancel", "job_id": first_job }),
                    "hang_cancel",
                )]
            } else {
                vec![make_tool_call(
                    "finish_task",
                    json!({ "summary": "cancelled hanging job" }),
                    "hang_finish_2",
                )]
            }
        } else if user_text.contains("continue after cancellation") {
            vec![make_tool_call(
                "finish_task",
                json!({ "summary": "continued after cancellation" }),
                "hang_finish_3",
            )]
        } else if user_text.contains("continue after background work") {
            let summary = if system_text.contains("Background subagent updates are available") {
                "noticed background update"
            } else {
                "missed background update"
            };
            vec![make_tool_call(
                "finish_task",
                json!({ "summary": summary }),
                "notice_finish",
            )]
        } else {
            vec![make_tool_call(
                "finish_task",
                json!({ "summary": "no-op" }),
                "parent_default_finish",
            )]
        };

        for event in events {
            let _ = tx.try_send(event);
        }
        let _ = tx.try_send(StreamEvent::Done);

        Ok(rx)
    }

    fn subagent_stream(user_text: String) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
        if user_text.contains("force fail") {
            return Err(LlmError::ApiError(
                "forced subagent failure for integration test".to_string(),
            ));
        }

        let (tx, rx) = mpsc::channel(8);
        if user_text.contains("hang forever") {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(60)).await;
                drop(tx);
            });
            return Ok(rx);
        }

        let _ = tx.try_send(make_tool_call(
            "finish_task",
            json!({
                "summary": format!("completed {}", user_text)
            }),
            "subagent_finish",
        ));
        let _ = tx.try_send(StreamEvent::Done);
        Ok(rx)
    }

    fn make_tool_call(name: &str, args: Value, id: &str) -> StreamEvent {
        StreamEvent::ToolCall(
            FunctionCall {
                name: name.to_string(),
                args,
                id: Some(id.to_string()),
            },
            None,
        )
    }

    fn session_dir(session_id: &str) -> std::path::PathBuf {
        crate::schema::StoragePaths::session_dir(session_id)
    }

    fn cleanup_session_artifacts(session_id: &str) {
        let _ = std::fs::remove_dir_all(session_dir(session_id));
        // Also remove the transcript .jsonl file which lives alongside the session dir
        let transcript = std::path::PathBuf::from("rusty_claw")
            .join("sessions")
            .join(format!("{}.jsonl", session_id));
        let _ = std::fs::remove_file(&transcript);
        // Remove task state
        let task_state = crate::schema::StoragePaths::task_state_file(session_id);
        let _ = std::fs::remove_file(&task_state);
    }

    async fn wait_for_jobs(runtime: &crate::subagent_runtime::SubagentRuntime, expected: usize) {
        for _ in 0..1000 {
            if runtime.list_jobs().await.len() == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let jobs = runtime.list_jobs().await;
        panic!(
            "timed out waiting for {} jobs. Current jobs: {:?}",
            expected, jobs
        );
    }

    async fn wait_for_all_terminal(runtime: &crate::subagent_runtime::SubagentRuntime) {
        for _ in 0..1000 {
            let jobs = runtime.list_jobs().await;
            if !jobs.is_empty() && jobs.iter().all(|job| job.state.is_terminal()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for subagent jobs to finish");
    }

    async fn wait_for_cancelled(runtime: &crate::subagent_runtime::SubagentRuntime, job_id: &str) {
        for _ in 0..1000 {
            let snapshot = runtime.get_job_snapshot(job_id, false).await.unwrap();
            if matches!(
                snapshot.state,
                crate::subagent_runtime::SubagentJobState::Cancelled { .. }
            ) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for cancelled state");
    }

    fn extract_tool_payloads(tool_results: &[String], tool_name: &str) -> Vec<serde_json::Value> {
        tool_results
            .iter()
            .filter_map(|result| ToolExecutionEnvelope::from_json_str(result))
            .filter(|envelope| envelope.result.tool_name == tool_name)
            .filter_map(|envelope| serde_json::from_str::<Value>(&envelope.result.output).ok())
            .collect()
    }

    #[tokio::test]
    #[ignore = "timing-sensitive session-manager integration scenario"]
    async fn test_parent_session_can_spawn_and_collect_background_subagent() {
        let _guard = SESSION_MANAGER_TEST_MUTEX
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let session_id = "integration_async_subagent_spawn_collect_one";
        cleanup_session_artifacts(session_id);

        let manager = SessionManager::new(
            Some(Arc::new(AsyncSubagentScenarioLlm)),
            vec![Arc::new(MockReadTool)],
        );
        let output = Arc::new(CaptureOutput::default());
        let agent = manager
            .get_or_create_session(session_id, "test_cli", output.clone())
            .await
            .unwrap();

        let exit = agent
            .lock()
            .await
            .step("spawn one background job".to_string())
            .await
            .unwrap();
        match exit {
            RunExit::Finished(summary) => assert!(summary.starts_with("spawned background job")),
            other => panic!("expected finished exit, got {:?}", other),
        }

        let runtime = manager.subagent_runtime.read().unwrap().clone().unwrap();
        wait_for_jobs(&runtime, 1).await;
        wait_for_all_terminal(&runtime).await;

        let exit = agent
            .lock()
            .await
            .step("collect one background job".to_string())
            .await
            .unwrap();
        match exit {
            RunExit::Finished(summary) => assert!(summary.starts_with("collected background job")),
            other => panic!("expected finished exit, got {:?}", other),
        }

        let tool_results = output.tool_results();
        let result_payloads = extract_tool_payloads(&tool_results, "subagent");
        assert!(result_payloads.len() >= 1);
        assert!(
            result_payloads
                .iter()
                .filter(|p| p.get("status").and_then(Value::as_str) == Some("finished"))
                .count()
                >= 1
        );

        cleanup_session_artifacts(session_id);
    }

    #[tokio::test]
    #[ignore = "timing-sensitive session-manager integration scenario"]
    async fn test_parent_session_can_collect_failed_background_subagent() {
        let _guard = SESSION_MANAGER_TEST_MUTEX
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let session_id = "integration_async_subagent_failure_collect";
        cleanup_session_artifacts(session_id);

        let manager = SessionManager::new(
            Some(Arc::new(AsyncSubagentScenarioLlm)),
            vec![Arc::new(MockReadTool)],
        );
        let output = Arc::new(CaptureOutput::default());
        let agent = manager
            .get_or_create_session(session_id, "test_cli", output.clone())
            .await
            .unwrap();

        let exit = agent
            .lock()
            .await
            .step("spawn failing background job".to_string())
            .await
            .unwrap();
        match exit {
            RunExit::Finished(summary) => assert!(summary.starts_with("spawned failing job")),
            other => panic!("expected finished exit, got {:?}", other),
        }

        let runtime = manager.subagent_runtime.read().unwrap().clone().unwrap();
        wait_for_jobs(&runtime, 1).await;
        wait_for_all_terminal(&runtime).await;

        let exit = agent
            .lock()
            .await
            .step("collect failing background job".to_string())
            .await
            .unwrap();
        match exit {
            RunExit::Finished(summary) => assert!(summary.starts_with("collected failing job")),
            other => panic!("expected finished exit, got {:?}", other),
        }

        let tool_results = output.tool_results();
        let result_payloads = extract_tool_payloads(&tool_results, "subagent");
        assert!(result_payloads.len() >= 1);
        assert!(result_payloads
            .iter()
            .any(|p| p.get("status").and_then(Value::as_str) == Some("failed")));

        cleanup_session_artifacts(session_id);
    }

    #[tokio::test]
    #[ignore = "timing-sensitive session-manager integration scenario"]
    async fn test_cancel_subagent_does_not_block_parent_session_progress() {
        let _guard = SESSION_MANAGER_TEST_MUTEX
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let session_id = "integration_async_subagent_cancel";
        cleanup_session_artifacts(session_id);

        let manager = SessionManager::new(
            Some(Arc::new(AsyncSubagentScenarioLlm)),
            vec![Arc::new(MockReadTool)],
        );
        let output = Arc::new(CaptureOutput::default());
        let agent = manager
            .get_or_create_session(session_id, "test_cli", output)
            .await
            .unwrap();
        let tool_ctx = ToolContext::new(session_id, "test_cli");

        let exit = agent
            .lock()
            .await
            .step("spawn hanging job".to_string())
            .await
            .unwrap();
        match exit {
            RunExit::Finished(summary) => assert!(summary.starts_with("spawned hanging job")),
            other => panic!("expected finished exit, got {:?}", other),
        }

        let runtime = manager.subagent_runtime.read().unwrap().clone().unwrap();
        wait_for_jobs(&runtime, 1).await;
        let spawned_job_id = runtime.list_jobs().await[0].meta.job_id.clone();

        let start = Instant::now();
        let cancel_result = crate::tools::SubagentTool::new(runtime.clone())
            .execute(
                json!({ "action": "cancel", "job_id": spawned_job_id }),
                &tool_ctx,
            )
            .await
            .unwrap();
        let cancel_envelope = ToolExecutionEnvelope::from_json_str(&cancel_result).unwrap();
        assert_eq!(cancel_envelope.result.tool_name, "subagent");
        assert!(cancel_envelope.result.ok);
        wait_for_cancelled(&runtime, &spawned_job_id).await;

        let exit = agent
            .lock()
            .await
            .step("continue after cancellation".to_string())
            .await
            .unwrap();
        match exit {
            RunExit::Finished(summary) => {
                assert!(summary.starts_with("continued after cancellation"))
            }
            other => panic!("expected finished exit, got {:?}", other),
        }
        assert!(start.elapsed() < Duration::from_secs(15));

        cleanup_session_artifacts(session_id);
    }

    #[tokio::test]
    #[ignore = "timing-sensitive session-manager integration scenario"]
    async fn test_parent_session_receives_background_completion_notice_on_next_turn() {
        let _guard = SESSION_MANAGER_TEST_MUTEX
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let session_id = "integration_async_subagent_notification_single";
        cleanup_session_artifacts(session_id);

        let manager = SessionManager::new(
            Some(Arc::new(AsyncSubagentScenarioLlm)),
            vec![Arc::new(MockReadTool)],
        );
        let output = Arc::new(CaptureOutput::default());
        let agent = manager
            .get_or_create_session(session_id, "test_cli", output)
            .await
            .unwrap();

        let exit = agent
            .lock()
            .await
            .step("spawn one background job".to_string())
            .await
            .unwrap();
        match exit {
            RunExit::Finished(summary) => assert!(summary.starts_with("spawned background job")),
            other => panic!("expected finished exit, got {:?}", other),
        }

        let runtime = manager.subagent_runtime.read().unwrap().clone().unwrap();
        wait_for_jobs(&runtime, 1).await;
        wait_for_all_terminal(&runtime).await;

        let exit = agent
            .lock()
            .await
            .step("continue after background work".to_string())
            .await
            .unwrap();
        match exit {
            RunExit::Finished(summary) => assert!(summary.starts_with("noticed background update")),
            other => panic!("expected finished exit, got {:?}", other),
        }

        cleanup_session_artifacts(session_id);
    }
}
