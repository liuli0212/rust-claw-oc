use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::llm_client::LlmClient;
use crate::session::factory::{build_subagent_session, BuiltSubagentSession, SubagentBuildMode};
use crate::tools::protocol::ToolError;
use crate::tools::subagent::{DispatchSubagentArgs, SubagentResult};
use crate::tools::{Tool, ToolContext};

const UNCONSUMED_TERMINAL_JOB_TTL: Duration = Duration::from_secs(30 * 60);
const CONSUMED_TERMINAL_JOB_TTL: Duration = Duration::from_secs(5 * 60);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone)]
pub struct SpawnedSubagentJob {
    pub job_id: String,
    pub sub_session_id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubagentJobMeta {
    pub job_id: String,
    pub parent_session_id: String,
    pub parent_reply_to: String,
    pub sub_session_id: String,
    pub goal: String,
    pub input_summary: String,
    pub allowed_tools: Vec<String>,
    pub claimed_paths: Vec<String>,
    pub allow_writes: bool,
    pub timeout_sec: u64,
    pub max_steps: usize,
    pub created_at_unix_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum SubagentJobState {
    Pending,
    Running {
        started_at_unix_ms: u64,
    },
    Completed {
        finished_at_unix_ms: u64,
        result: SubagentResult,
    },
    Failed {
        finished_at_unix_ms: u64,
        error: String,
        partial: Option<SubagentResult>,
    },
    Cancelled {
        finished_at_unix_ms: u64,
        partial: Option<SubagentResult>,
    },
    TimedOut {
        finished_at_unix_ms: u64,
        partial: Option<SubagentResult>,
    },
}

impl SubagentJobState {
    pub fn finish_reason(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running { .. } => "running",
            Self::Completed { .. } => "finished",
            Self::Failed { .. } => "failed",
            Self::Cancelled { .. } => "cancelled",
            Self::TimedOut { .. } => "timed_out",
        }
    }

    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Pending | Self::Running { .. })
    }

    fn finished_at_unix_ms(&self) -> Option<u64> {
        match self {
            Self::Completed {
                finished_at_unix_ms,
                ..
            }
            | Self::Failed {
                finished_at_unix_ms,
                ..
            }
            | Self::Cancelled {
                finished_at_unix_ms,
                ..
            }
            | Self::TimedOut {
                finished_at_unix_ms,
                ..
            } => Some(*finished_at_unix_ms),
            Self::Pending | Self::Running { .. } => None,
        }
    }
}

pub struct SubagentJobHandle {
    pub meta: SubagentJobMeta,
    pub state: tokio::sync::RwLock<SubagentJobState>,
    pub consumed_at_unix_ms: tokio::sync::RwLock<Option<u64>>,
    pub cancelled: Arc<AtomicBool>,
    pub cancel_notify: Arc<tokio::sync::Notify>,
    pub task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl SubagentJobHandle {
    fn new(meta: SubagentJobMeta) -> Self {
        Self {
            meta,
            state: tokio::sync::RwLock::new(SubagentJobState::Pending),
            consumed_at_unix_ms: tokio::sync::RwLock::new(None),
            cancelled: Arc::new(AtomicBool::new(false)),
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
            task: tokio::sync::Mutex::new(None),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubagentJobSnapshot {
    pub meta: SubagentJobMeta,
    pub state: SubagentJobState,
    pub consumed: bool,
    pub consumed_at_unix_ms: Option<u64>,
}

struct RunningJobGuard {
    counter: Arc<AtomicUsize>,
}

impl RunningJobGuard {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self { counter }
    }
}

impl Drop for RunningJobGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone)]
pub struct SubagentRuntime {
    inner: Arc<SubagentRuntimeInner>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SubagentNotification {
    pub job_id: String,
    pub sub_session_id: String,
    pub status: String,
    pub summary: String,
}

struct SubagentRuntimeInner {
    jobs: tokio::sync::RwLock<HashMap<String, Arc<SubagentJobHandle>>>,
    notifications: tokio::sync::RwLock<HashMap<String, Vec<SubagentNotification>>>,
    running_jobs: Arc<AtomicUsize>,
    max_concurrent_jobs: usize,
    llm: Arc<dyn LlmClient>,
    base_tools: Vec<Arc<dyn Tool>>,
}

impl SubagentRuntime {
    pub fn new(
        llm: Arc<dyn LlmClient>,
        base_tools: Vec<Arc<dyn Tool>>,
        max_concurrent_jobs: usize,
    ) -> Self {
        let runtime = Self {
            inner: Arc::new(SubagentRuntimeInner {
                jobs: tokio::sync::RwLock::new(HashMap::new()),
                notifications: tokio::sync::RwLock::new(HashMap::new()),
                running_jobs: Arc::new(AtomicUsize::new(0)),
                max_concurrent_jobs: max_concurrent_jobs.max(1),
                llm,
                base_tools,
            }),
        };

        let cleanup_runtime = runtime.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(CLEANUP_INTERVAL);
            loop {
                interval.tick().await;
                cleanup_runtime.cleanup_expired_jobs().await;
            }
        });

        runtime
    }

    pub async fn spawn_job(
        &self,
        parent_ctx: ToolContext,
        args: DispatchSubagentArgs,
    ) -> Result<SpawnedSubagentJob, ToolError> {
        self.cleanup_expired_jobs().await;

        if self.inner.running_jobs.load(Ordering::SeqCst) >= self.inner.max_concurrent_jobs {
            return Err(ToolError::ExecutionFailed(
                "Too many concurrent subagent jobs. Wait for existing jobs to finish before spawning more.".to_string(),
            ));
        }

        let timeout_sec = args.timeout_sec.unwrap_or(60);
        let max_steps = args.max_steps.unwrap_or(5).max(1);
        let job_id = format!("subjob_{}", uuid::Uuid::new_v4().simple());
        let sub_session_id = format!(
            "sub_{}_{}",
            parent_ctx.session_id,
            uuid::Uuid::new_v4().simple()
        );
        let meta = SubagentJobMeta {
            job_id: job_id.clone(),
            parent_session_id: parent_ctx.session_id.clone(),
            parent_reply_to: parent_ctx.reply_to.clone(),
            sub_session_id: sub_session_id.clone(),
            goal: args.goal.clone(),
            input_summary: args.input_summary.clone(),
            allowed_tools: args.allowed_tools.clone(),
            claimed_paths: normalize_claimed_paths(&args.claimed_paths),
            allow_writes: args.allow_writes,
            timeout_sec,
            max_steps,
            created_at_unix_ms: unix_ms_now(),
        };

        if meta.allow_writes && meta.claimed_paths.is_empty() {
            return Err(ToolError::ExecutionFailed(
                "Background subagents with allow_writes=true must declare at least one claimed path."
                    .to_string(),
            ));
        }

        if let Some((conflicting_job_id, claimed_path)) =
            self.find_claimed_path_conflict(&meta.claimed_paths).await
        {
            return Err(ToolError::ExecutionFailed(format!(
                "Claimed path conflict for '{}'. Active subagent job '{}' already owns an overlapping path. Wait for it to finish or choose a non-overlapping path range.",
                claimed_path, conflicting_job_id
            )));
        }

        let handle = Arc::new(SubagentJobHandle::new(meta));
        {
            let mut jobs = self.inner.jobs.write().await;
            jobs.insert(job_id.clone(), handle.clone());
        }

        let runtime = self.clone();
        let counter = self.inner.running_jobs.clone();
        let running_guard = RunningJobGuard::new(counter);
        let handle_for_task = handle.clone();
        let sub_session_id_for_task = sub_session_id.clone();
        let join_handle = tokio::spawn(async move {
            let _guard = running_guard;
            runtime
                .run_job(handle_for_task, parent_ctx, args, sub_session_id_for_task)
                .await;
        });
        *handle.task.lock().await = Some(join_handle);

        Ok(SpawnedSubagentJob {
            job_id,
            sub_session_id,
        })
    }

    pub async fn get_job_snapshot(
        &self,
        job_id: &str,
        consume: bool,
    ) -> Result<SubagentJobSnapshot, ToolError> {
        self.cleanup_expired_jobs().await;
        let handle = self.get_job_handle(job_id).await.ok_or_else(|| {
            ToolError::ExecutionFailed(format!("Unknown subagent job: {}", job_id))
        })?;
        let state = handle.state.read().await.clone();
        let consumed_at_unix_ms = if consume && state.is_terminal() {
            let mut consumed_at = handle.consumed_at_unix_ms.write().await;
            Some(*consumed_at.get_or_insert_with(unix_ms_now))
        } else {
            *handle.consumed_at_unix_ms.read().await
        };
        Ok(SubagentJobSnapshot {
            meta: handle.meta.clone(),
            state,
            consumed: consumed_at_unix_ms.is_some(),
            consumed_at_unix_ms,
        })
    }

    pub async fn list_jobs(&self) -> Vec<SubagentJobSnapshot> {
        self.cleanup_expired_jobs().await;
        let handles: Vec<Arc<SubagentJobHandle>> = {
            let jobs = self.inner.jobs.read().await;
            jobs.values().cloned().collect()
        };

        let mut snapshots = Vec::with_capacity(handles.len());
        for handle in handles {
            let consumed_at_unix_ms = *handle.consumed_at_unix_ms.read().await;
            snapshots.push(SubagentJobSnapshot {
                meta: handle.meta.clone(),
                state: handle.state.read().await.clone(),
                consumed: consumed_at_unix_ms.is_some(),
                consumed_at_unix_ms,
            });
        }
        snapshots.sort_by_key(|snapshot| snapshot.meta.created_at_unix_ms);
        snapshots
    }

    pub async fn take_notifications(&self, parent_session_id: &str) -> Vec<SubagentNotification> {
        let mut notifications = self.inner.notifications.write().await;
        notifications.remove(parent_session_id).unwrap_or_default()
    }

    pub async fn cancel_job(&self, job_id: &str) -> Result<(), ToolError> {
        self.cleanup_expired_jobs().await;
        let handle = self.get_job_handle(job_id).await.ok_or_else(|| {
            ToolError::ExecutionFailed(format!("Unknown subagent job: {}", job_id))
        })?;
        handle.cancelled.store(true, Ordering::SeqCst);
        handle.cancel_notify.notify_waiters();
        if let Some(task) = handle.task.lock().await.as_ref() {
            task.abort();
        }
        let mut should_notify = None;
        {
            let mut state = handle.state.write().await;
            if !state.is_terminal() {
                *state = SubagentJobState::Cancelled {
                    finished_at_unix_ms: unix_ms_now(),
                    partial: None,
                };
                should_notify = Some(state.clone());
            }
        }
        if let Some(state) = should_notify {
            self.enqueue_notification(&handle.meta, &state).await;
        }
        Ok(())
    }

    pub async fn cleanup_expired_jobs(&self) {
        let expired_ids: Vec<String> = {
            let jobs = self.inner.jobs.read().await;
            jobs.iter()
                .filter_map(|(job_id, handle)| {
                    let state = handle.state.try_read().ok()?;
                    let finished_at = state.finished_at_unix_ms()?;
                    let consumed_at_unix_ms = handle
                        .consumed_at_unix_ms
                        .try_read()
                        .ok()
                        .and_then(|value| *value);
                    let age_ms = unix_ms_now().saturating_sub(finished_at);
                    let ttl = if consumed_at_unix_ms.is_some() {
                        CONSUMED_TERMINAL_JOB_TTL
                    } else {
                        UNCONSUMED_TERMINAL_JOB_TTL
                    };
                    if age_ms >= ttl.as_millis() as u64 {
                        Some(job_id.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };

        if expired_ids.is_empty() {
            return;
        }

        let mut jobs = self.inner.jobs.write().await;
        for job_id in expired_ids {
            jobs.remove(&job_id);
        }
    }

    async fn get_job_handle(&self, job_id: &str) -> Option<Arc<SubagentJobHandle>> {
        let jobs = self.inner.jobs.read().await;
        jobs.get(job_id).cloned()
    }

    async fn find_claimed_path_conflict(
        &self,
        claimed_paths: &[String],
    ) -> Option<(String, String)> {
        if claimed_paths.is_empty() {
            return None;
        }

        let handles: Vec<Arc<SubagentJobHandle>> = {
            let jobs = self.inner.jobs.read().await;
            jobs.values().cloned().collect()
        };

        for handle in handles {
            let state = handle.state.read().await;
            if state.is_terminal() {
                continue;
            }
            drop(state);

            for existing in &handle.meta.claimed_paths {
                for incoming in claimed_paths {
                    if claimed_paths_overlap(existing, incoming) {
                        return Some((handle.meta.job_id.clone(), incoming.clone()));
                    }
                }
            }
        }

        None
    }

    async fn run_job(
        &self,
        handle: Arc<SubagentJobHandle>,
        parent_ctx: ToolContext,
        args: DispatchSubagentArgs,
        sub_session_id: String,
    ) {
        {
            let mut state = handle.state.write().await;
            *state = SubagentJobState::Running {
                started_at_unix_ms: unix_ms_now(),
            };
        }

        let final_state = match build_subagent_session(
            &parent_ctx,
            self.inner.llm.clone(),
            &self.inner.base_tools,
            if handle.meta.allow_writes {
                SubagentBuildMode::AsyncControlledWrite
            } else {
                SubagentBuildMode::AsyncReadonly
            },
            Some(sub_session_id),
            &args.allowed_tools,
            args.max_steps.unwrap_or(5).max(1),
            &args.input_summary,
            handle.cancelled.clone(),
            handle.cancel_notify.clone(),
        ) {
            Ok(BuiltSubagentSession {
                mut agent_loop,
                collector,
                ..
            }) => {
                self.execute_subagent(handle.clone(), args, collector, &mut agent_loop)
                    .await
            }
            Err(error) => SubagentJobState::Failed {
                finished_at_unix_ms: unix_ms_now(),
                error,
                partial: None,
            },
        };

        self.enqueue_notification(&handle.meta, &final_state).await;
        let mut state = handle.state.write().await;
        *state = final_state;
    }

    async fn enqueue_notification(&self, meta: &SubagentJobMeta, final_state: &SubagentJobState) {
        if !final_state.is_terminal() {
            return;
        }

        let summary = match final_state {
            SubagentJobState::Completed { result, .. } => result.summary.clone(),
            SubagentJobState::Failed { error, partial, .. } => partial
                .as_ref()
                .map(|result| result.summary.clone())
                .unwrap_or_else(|| error.clone()),
            SubagentJobState::Cancelled { partial, .. } => partial
                .as_ref()
                .map(|result| result.summary.clone())
                .unwrap_or_else(|| "Sub-agent execution was interrupted.".to_string()),
            SubagentJobState::TimedOut { partial, .. } => partial
                .as_ref()
                .map(|result| result.summary.clone())
                .unwrap_or_else(|| "Sub-agent timed out.".to_string()),
            SubagentJobState::Pending | SubagentJobState::Running { .. } => return,
        };

        let notification = SubagentNotification {
            job_id: meta.job_id.clone(),
            sub_session_id: meta.sub_session_id.clone(),
            status: final_state.finish_reason().to_string(),
            summary,
        };

        let mut notifications = self.inner.notifications.write().await;
        notifications
            .entry(meta.parent_session_id.clone())
            .or_default()
            .push(notification);
    }

    #[cfg(test)]
    pub async fn record_notification_for_test(
        &self,
        parent_session_id: &str,
        notification: SubagentNotification,
    ) {
        let mut notifications = self.inner.notifications.write().await;
        notifications
            .entry(parent_session_id.to_string())
            .or_default()
            .push(notification);
    }

    async fn execute_subagent(
        &self,
        handle: Arc<SubagentJobHandle>,
        args: DispatchSubagentArgs,
        collector: Arc<crate::session::factory::CollectorOutput>,
        agent_loop: &mut crate::core::AgentLoop,
    ) -> SubagentJobState {
        let run_result = tokio::time::timeout(
            Duration::from_secs(args.timeout_sec.unwrap_or(60)),
            agent_loop.step(args.goal.clone()),
        )
        .await;

        let collected_text = collector.take_text().await;
        let tool_outputs = collector.take_tool_outputs().await;
        let artifacts = collector.take_artifacts().await;
        let finished_at_unix_ms = unix_ms_now();

        match run_result {
            Ok(Ok(exit)) => {
                let ok = matches!(exit, crate::core::RunExit::Finished(_));
                let summary = match exit {
                    crate::core::RunExit::Finished(summary) => summary,
                    crate::core::RunExit::YieldedToUser => {
                        if collected_text.trim().is_empty() {
                            "Sub-agent yielded without visible output.".to_string()
                        } else {
                            format!("Sub-agent yielded with output: {}", collected_text.trim())
                        }
                    }
                    crate::core::RunExit::RecoverableFailed(message)
                    | crate::core::RunExit::CriticallyFailed(message)
                    | crate::core::RunExit::AutopilotStalled(message) => {
                        return SubagentJobState::Failed {
                            finished_at_unix_ms,
                            error: message.clone(),
                            partial: Some(SubagentResult {
                                ok,
                                summary: message,
                                findings: tool_outputs,
                                artifacts,
                            }),
                        };
                    }
                    crate::core::RunExit::StoppedByUser => {
                        return SubagentJobState::Cancelled {
                            finished_at_unix_ms,
                            partial: Some(SubagentResult {
                                ok: false,
                                summary: "Sub-agent execution was interrupted.".to_string(),
                                findings: tool_outputs,
                                artifacts,
                            }),
                        };
                    }
                };

                SubagentJobState::Completed {
                    finished_at_unix_ms,
                    result: SubagentResult {
                        ok,
                        summary,
                        findings: tool_outputs,
                        artifacts,
                    },
                }
            }
            Ok(Err(error)) => SubagentJobState::Failed {
                finished_at_unix_ms,
                error: error.to_string(),
                partial: Some(SubagentResult {
                    ok: false,
                    summary: format!("Sub-agent error: {}", error),
                    findings: tool_outputs,
                    artifacts,
                }),
            },
            Err(_) => {
                if handle.cancelled.load(Ordering::SeqCst) {
                    SubagentJobState::Cancelled {
                        finished_at_unix_ms,
                        partial: Some(SubagentResult {
                            ok: false,
                            summary: "Sub-agent execution was interrupted.".to_string(),
                            findings: tool_outputs,
                            artifacts,
                        }),
                    }
                } else {
                    SubagentJobState::TimedOut {
                        finished_at_unix_ms,
                        partial: Some(SubagentResult {
                            ok: false,
                            summary: format!(
                                "Sub-agent timed out after {}s while working on '{}'.",
                                args.timeout_sec.unwrap_or(60),
                                args.goal
                            ),
                            findings: tool_outputs,
                            artifacts,
                        }),
                    }
                }
            }
        }
    }
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn normalize_claimed_paths(paths: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    for path in paths {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut canonical = trimmed.replace('\\', "/");
        while canonical.ends_with('/') && canonical.len() > 1 {
            canonical.pop();
        }
        if canonical == "." {
            canonical.clear();
        }
        if canonical.is_empty() {
            continue;
        }
        if !normalized.iter().any(|existing| existing == &canonical) {
            normalized.push(canonical);
        }
    }
    normalized
}

fn claimed_paths_overlap(left: &str, right: &str) -> bool {
    let left = left.trim_matches('/');
    let right = right.trim_matches('/');
    left == right
        || left.starts_with(&format!("{right}/"))
        || right.starts_with(&format!("{left}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use serde_json::json;
    use tokio::sync::mpsc;

    use crate::context::{FunctionCall, Message};
    use crate::llm_client::{LlmError, StreamEvent};

    struct FinishImmediatelyLlm;

    #[async_trait]
    impl LlmClient for FinishImmediatelyLlm {
        fn model_name(&self) -> &str {
            "finish-immediately"
        }

        fn provider_name(&self) -> &str {
            "test"
        }

        async fn stream(
            &self,
            _messages: Vec<Message>,
            _system_instruction: Option<Message>,
            _tools: Vec<Arc<dyn Tool>>,
        ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
            let (tx, rx) = mpsc::channel(4);
            tokio::spawn(async move {
                let _ = tx
                    .send(StreamEvent::ToolCall(
                        FunctionCall {
                            name: "finish_task".to_string(),
                            args: json!({ "summary": "done" }),
                            id: Some("tc_1".to_string()),
                        },
                        None,
                    ))
                    .await;
                let _ = tx.send(StreamEvent::Done).await;
            });
            Ok(rx)
        }
    }

    struct HangingLlm;

    #[async_trait]
    impl LlmClient for HangingLlm {
        fn model_name(&self) -> &str {
            "hanging"
        }

        fn provider_name(&self) -> &str {
            "test"
        }

        async fn stream(
            &self,
            _messages: Vec<Message>,
            _system_instruction: Option<Message>,
            _tools: Vec<Arc<dyn Tool>>,
        ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
            let (tx, rx) = mpsc::channel(4);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(60)).await;
                drop(tx);
            });
            Ok(rx)
        }
    }

    struct MockTool(&'static str);

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> String {
            self.0.to_string()
        }

        fn description(&self) -> String {
            String::new()
        }

        fn parameters_schema(&self) -> serde_json::Value {
            json!({})
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<String, ToolError> {
            Ok(String::new())
        }
    }

    fn make_ctx() -> ToolContext {
        ToolContext {
            session_id: "parent".to_string(),
            reply_to: "cli".to_string(),
        }
    }

    async fn wait_for_terminal_state(runtime: &SubagentRuntime, job_id: &str) -> SubagentJobState {
        for _ in 0..40 {
            let snapshot = runtime.get_job_snapshot(job_id, false).await.unwrap();
            if snapshot.state.is_terminal() {
                return snapshot.state;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("job did not reach terminal state in time");
    }

    #[tokio::test]
    async fn test_spawn_job_completes_and_records_result() {
        let runtime = SubagentRuntime::new(
            Arc::new(FinishImmediatelyLlm),
            vec![Arc::new(MockTool("read_file"))],
            2,
        );

        let spawned = runtime
            .spawn_job(
                make_ctx(),
                DispatchSubagentArgs {
                    goal: "inspect".to_string(),
                    input_summary: "summary".to_string(),
                    allowed_tools: vec!["read_file".to_string()],
                    claimed_paths: Vec::new(),
                    allow_writes: false,
                    timeout_sec: Some(5),
                    max_steps: Some(4),
                },
            )
            .await
            .unwrap();

        let state = wait_for_terminal_state(&runtime, &spawned.job_id).await;
        match state {
            SubagentJobState::Completed { result, .. } => {
                assert!(result.ok);
                assert!(result.summary.contains("done"));
            }
            other => panic!("expected completed state, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_spawn_job_respects_concurrency_limit() {
        let runtime = SubagentRuntime::new(
            Arc::new(HangingLlm),
            vec![Arc::new(MockTool("read_file"))],
            1,
        );

        let first = runtime
            .spawn_job(
                make_ctx(),
                DispatchSubagentArgs {
                    goal: "hang".to_string(),
                    input_summary: "summary".to_string(),
                    allowed_tools: vec!["read_file".to_string()],
                    claimed_paths: Vec::new(),
                    allow_writes: false,
                    timeout_sec: Some(30),
                    max_steps: Some(4),
                },
            )
            .await
            .unwrap();

        let second = runtime
            .spawn_job(
                make_ctx(),
                DispatchSubagentArgs {
                    goal: "blocked".to_string(),
                    input_summary: "summary".to_string(),
                    allowed_tools: vec!["read_file".to_string()],
                    claimed_paths: Vec::new(),
                    allow_writes: false,
                    timeout_sec: Some(30),
                    max_steps: Some(4),
                },
            )
            .await;

        assert!(matches!(second, Err(ToolError::ExecutionFailed(_))));

        runtime.cancel_job(&first.job_id).await.unwrap();
        let state = wait_for_terminal_state(&runtime, &first.job_id).await;
        assert!(matches!(state, SubagentJobState::Cancelled { .. }));
    }

    #[tokio::test]
    async fn test_cancel_job_transitions_running_job_to_cancelled() {
        let runtime = SubagentRuntime::new(
            Arc::new(HangingLlm),
            vec![Arc::new(MockTool("read_file"))],
            1,
        );

        let spawned = runtime
            .spawn_job(
                make_ctx(),
                DispatchSubagentArgs {
                    goal: "hang".to_string(),
                    input_summary: "summary".to_string(),
                    allowed_tools: vec!["read_file".to_string()],
                    claimed_paths: Vec::new(),
                    allow_writes: false,
                    timeout_sec: Some(30),
                    max_steps: Some(4),
                },
            )
            .await
            .unwrap();

        runtime.cancel_job(&spawned.job_id).await.unwrap();
        let state = wait_for_terminal_state(&runtime, &spawned.job_id).await;
        assert!(matches!(state, SubagentJobState::Cancelled { .. }));
    }

    #[tokio::test]
    async fn test_spawn_job_rejects_overlapping_claimed_paths() {
        let runtime = SubagentRuntime::new(
            Arc::new(HangingLlm),
            vec![Arc::new(MockTool("read_file"))],
            2,
        );

        let first = runtime
            .spawn_job(
                make_ctx(),
                DispatchSubagentArgs {
                    goal: "own parser".to_string(),
                    input_summary: "summary".to_string(),
                    allowed_tools: vec!["read_file".to_string()],
                    claimed_paths: vec!["src/parser".to_string()],
                    allow_writes: false,
                    timeout_sec: Some(30),
                    max_steps: Some(4),
                },
            )
            .await
            .unwrap();

        let second = runtime
            .spawn_job(
                make_ctx(),
                DispatchSubagentArgs {
                    goal: "touch parser child".to_string(),
                    input_summary: "summary".to_string(),
                    allowed_tools: vec!["read_file".to_string()],
                    claimed_paths: vec!["src/parser/ast".to_string()],
                    allow_writes: false,
                    timeout_sec: Some(30),
                    max_steps: Some(4),
                },
            )
            .await;

        match second {
            Err(ToolError::ExecutionFailed(message)) => {
                assert!(message.contains("Claimed path conflict"));
                assert!(message.contains(&first.job_id));
            }
            other => panic!("expected claimed path conflict, got {:?}", other),
        }

        runtime.cancel_job(&first.job_id).await.unwrap();
        let _ = wait_for_terminal_state(&runtime, &first.job_id).await;
    }

    #[tokio::test]
    async fn test_get_job_snapshot_can_mark_terminal_job_consumed() {
        let runtime = SubagentRuntime::new(
            Arc::new(FinishImmediatelyLlm),
            vec![Arc::new(MockTool("read_file"))],
            2,
        );

        let spawned = runtime
            .spawn_job(
                make_ctx(),
                DispatchSubagentArgs {
                    goal: "inspect".to_string(),
                    input_summary: "summary".to_string(),
                    allowed_tools: vec!["read_file".to_string()],
                    claimed_paths: Vec::new(),
                    allow_writes: false,
                    timeout_sec: Some(5),
                    max_steps: Some(4),
                },
            )
            .await
            .unwrap();

        let _ = wait_for_terminal_state(&runtime, &spawned.job_id).await;
        let first_snapshot = runtime
            .get_job_snapshot(&spawned.job_id, true)
            .await
            .unwrap();
        let second_snapshot = runtime
            .get_job_snapshot(&spawned.job_id, false)
            .await
            .unwrap();

        assert!(first_snapshot.consumed);
        assert!(first_snapshot.consumed_at_unix_ms.is_some());
        assert_eq!(
            first_snapshot.consumed_at_unix_ms,
            second_snapshot.consumed_at_unix_ms
        );
    }

    #[tokio::test]
    async fn test_cleanup_expired_jobs_uses_consumed_ttl() {
        let runtime = SubagentRuntime::new(
            Arc::new(FinishImmediatelyLlm),
            vec![Arc::new(MockTool("read_file"))],
            2,
        );

        let consumed_job = Arc::new(SubagentJobHandle::new(SubagentJobMeta {
            job_id: "consumed".to_string(),
            parent_session_id: "parent".to_string(),
            parent_reply_to: "cli".to_string(),
            sub_session_id: "sub_consumed".to_string(),
            goal: "inspect".to_string(),
            input_summary: "summary".to_string(),
            allowed_tools: vec!["read_file".to_string()],
            claimed_paths: Vec::new(),
            allow_writes: false,
            timeout_sec: 5,
            max_steps: 4,
            created_at_unix_ms: unix_ms_now(),
        }));
        {
            let mut state = consumed_job.state.write().await;
            *state = SubagentJobState::Completed {
                finished_at_unix_ms: unix_ms_now()
                    .saturating_sub(CONSUMED_TERMINAL_JOB_TTL.as_millis() as u64 + 1),
                result: SubagentResult {
                    ok: true,
                    summary: "done".to_string(),
                    findings: Vec::new(),
                    artifacts: Vec::new(),
                },
            };
        }
        {
            let mut consumed_at = consumed_job.consumed_at_unix_ms.write().await;
            *consumed_at = Some(unix_ms_now());
        }

        let unconsumed_job = Arc::new(SubagentJobHandle::new(SubagentJobMeta {
            job_id: "unconsumed".to_string(),
            parent_session_id: "parent".to_string(),
            parent_reply_to: "cli".to_string(),
            sub_session_id: "sub_unconsumed".to_string(),
            goal: "inspect".to_string(),
            input_summary: "summary".to_string(),
            allowed_tools: vec!["read_file".to_string()],
            claimed_paths: Vec::new(),
            allow_writes: false,
            timeout_sec: 5,
            max_steps: 4,
            created_at_unix_ms: unix_ms_now(),
        }));
        {
            let mut state = unconsumed_job.state.write().await;
            *state = SubagentJobState::Completed {
                finished_at_unix_ms: unix_ms_now()
                    .saturating_sub(CONSUMED_TERMINAL_JOB_TTL.as_millis() as u64 + 1),
                result: SubagentResult {
                    ok: true,
                    summary: "done".to_string(),
                    findings: Vec::new(),
                    artifacts: Vec::new(),
                },
            };
        }

        {
            let mut jobs = runtime.inner.jobs.write().await;
            jobs.insert("consumed".to_string(), consumed_job);
            jobs.insert("unconsumed".to_string(), unconsumed_job);
        }

        runtime.cleanup_expired_jobs().await;

        assert!(runtime.get_job_handle("consumed").await.is_none());
        assert!(runtime.get_job_handle("unconsumed").await.is_some());
    }

    #[tokio::test]
    async fn test_spawn_job_rejects_allow_writes_without_claimed_paths() {
        let runtime = SubagentRuntime::new(
            Arc::new(FinishImmediatelyLlm),
            vec![
                Arc::new(MockTool("read_file")),
                Arc::new(MockTool("write_file")),
            ],
            2,
        );

        let result = runtime
            .spawn_job(
                make_ctx(),
                DispatchSubagentArgs {
                    goal: "edit parser".to_string(),
                    input_summary: "summary".to_string(),
                    allowed_tools: vec!["write_file".to_string()],
                    claimed_paths: Vec::new(),
                    allow_writes: true,
                    timeout_sec: Some(5),
                    max_steps: Some(4),
                },
            )
            .await;

        match result {
            Err(ToolError::ExecutionFailed(message)) => {
                assert!(message.contains("allow_writes=true"));
            }
            other => panic!("expected allow_writes validation error, got {:?}", other),
        }
    }
}
