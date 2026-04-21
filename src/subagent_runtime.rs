use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::Instrument;

use crate::llm_client::LlmClient;
use crate::session::factory::{build_subagent_session, BuiltSubagentSession};
use crate::skills::call_tree::SkillSessionSeed;
use crate::tools::protocol::ToolError;
use crate::tools::subagent::SubagentResult;
use crate::tools::{Tool, ToolContext};
use crate::trace::{shared_bus, TraceActor, TraceContext, TraceSpanHandle, TraceStatus};
use futures::FutureExt;

const UNCONSUMED_TERMINAL_JOB_TTL: Duration = Duration::from_secs(30 * 60);
const CONSUMED_TERMINAL_JOB_TTL: Duration = Duration::from_secs(5 * 60);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const DEFAULT_SUBAGENT_TIMEOUT_SEC: u64 = 60;
pub const DEFAULT_SUBAGENT_MAX_STEPS: usize = 5;

#[derive(Debug, Clone)]
pub struct SpawnedSubagentJob {
    pub job_id: String,
    pub sub_session_id: String,
}

#[derive(Debug, Clone)]
pub struct SubagentExecutionRequest {
    pub initial_input: String,
    pub display_goal: String,
    pub context: String,
    pub timeout_sec: u64,
    pub max_steps: usize,
    pub allowed_tools: Vec<String>,
    pub restrict_to_allowed_tools: bool,
    pub allow_subagent_tool: bool,
    pub skill_name: Option<String>,
    pub lineage: Option<Vec<String>>,
    pub effective_tools: Option<Vec<String>>,
    pub effective_max_steps: Option<usize>,
    pub effective_timeout_sec: Option<u64>,
    pub skill_session_seed: SkillSessionSeed,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubagentJobMeta {
    pub job_id: String,
    pub parent_session_id: String,
    pub parent_reply_to: String,
    pub sub_session_id: String,
    pub goal: String,
    pub context: String,
    pub skill_name: Option<String>,
    pub created_at_unix_ms: u64,
    pub transcript_path: String,
    pub event_log_path: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SubagentDebugEvent {
    pub kind: String,
    pub tool_name: Option<String>,
    pub text: String,
    pub at_unix_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubagentDebugSnapshot {
    pub state_label: String,
    pub failure_stage: Option<String>,
    pub step_count: usize,
    pub last_model_text: Option<String>,
    pub last_thought_text: Option<String>,
    pub last_tool_name: Option<String>,
    pub last_tool_args_summary: Option<String>,
    pub last_tool_result_summary: Option<String>,
    pub last_error: Option<String>,
    pub recent_events: Vec<SubagentDebugEvent>,
    pub updated_at_unix_ms: u64,
}

impl Default for SubagentDebugSnapshot {
    fn default() -> Self {
        Self {
            state_label: "pending".to_string(),
            failure_stage: None,
            step_count: 0,
            last_model_text: None,
            last_thought_text: None,
            last_tool_name: None,
            last_tool_args_summary: None,
            last_tool_result_summary: None,
            last_error: None,
            recent_events: Vec::new(),
            updated_at_unix_ms: unix_ms_now(),
        }
    }
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
    pub debug: Arc<tokio::sync::RwLock<SubagentDebugSnapshot>>,
    pub consumed_at_unix_ms: tokio::sync::RwLock<Option<u64>>,
    pub cancelled: Arc<AtomicBool>,
    pub cancel_notify: Arc<tokio::sync::Notify>,
    /// Fired when the job reaches a terminal state (completed/failed/cancelled/timed_out).
    /// Separate from cancel_notify to avoid conflating cancellation and completion semantics.
    pub completion_notify: Arc<tokio::sync::Notify>,
    pub task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub trace_span: std::sync::Mutex<Option<TraceSpanHandle>>,
    pub trace_context: std::sync::Mutex<Option<crate::tools::protocol::ToolTraceContext>>,
}

impl SubagentJobHandle {
    fn new(meta: SubagentJobMeta) -> Self {
        Self {
            meta,
            state: tokio::sync::RwLock::new(SubagentJobState::Pending),
            debug: Arc::new(tokio::sync::RwLock::new(SubagentDebugSnapshot::default())),
            consumed_at_unix_ms: tokio::sync::RwLock::new(None),
            cancelled: Arc::new(AtomicBool::new(false)),
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
            completion_notify: Arc::new(tokio::sync::Notify::new()),
            task: tokio::sync::Mutex::new(None),
            trace_span: std::sync::Mutex::new(None),
            trace_context: std::sync::Mutex::new(None),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubagentJobSnapshot {
    pub meta: SubagentJobMeta,
    pub state: SubagentJobState,
    pub consumed: bool,
    pub consumed_at_unix_ms: Option<u64>,
    pub debug: SubagentDebugSnapshot,
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

struct SubagentJobRequest {
    parent_ctx: ToolContext,
    execution: SubagentExecutionRequest,
    sub_session_id: String,
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
        goal: String,
        context: String,
    ) -> Result<SpawnedSubagentJob, ToolError> {
        self.spawn_job_with_limits(
            parent_ctx,
            SubagentExecutionRequest {
                initial_input: goal.clone(),
                display_goal: goal,
                context,
                timeout_sec: DEFAULT_SUBAGENT_TIMEOUT_SEC,
                max_steps: DEFAULT_SUBAGENT_MAX_STEPS,
                allowed_tools: Vec::new(),
                restrict_to_allowed_tools: false,
                allow_subagent_tool: false,
                skill_name: None,
                lineage: None,
                effective_tools: None,
                effective_max_steps: None,
                effective_timeout_sec: None,
                skill_session_seed: SkillSessionSeed::default(),
            },
        )
        .await
    }

    pub(crate) async fn spawn_job_with_limits(
        &self,
        parent_ctx: ToolContext,
        execution: SubagentExecutionRequest,
    ) -> Result<SpawnedSubagentJob, ToolError> {
        self.cleanup_expired_jobs().await;

        if self.inner.running_jobs.load(Ordering::SeqCst) >= self.inner.max_concurrent_jobs {
            return Err(ToolError::ExecutionFailed(
                "Too many concurrent subagent jobs. Wait for existing jobs to finish before spawning more.".to_string(),
            ));
        }

        let unified_id = format!(
            "sub_{}_{}",
            parent_ctx.session_id,
            uuid::Uuid::new_v4().simple()
        );
        let meta = SubagentJobMeta {
            job_id: unified_id.clone(),
            parent_session_id: parent_ctx.session_id.clone(),
            parent_reply_to: parent_ctx.reply_to.clone(),
            sub_session_id: unified_id.clone(),
            goal: execution.display_goal.clone(),
            context: execution.context.clone(),
            skill_name: execution.skill_name.clone(),
            created_at_unix_ms: unix_ms_now(),
            transcript_path: crate::schema::StoragePaths::session_transcript_file(&unified_id)
                .display()
                .to_string(),
            event_log_path: crate::schema::StoragePaths::events_file(&unified_id)
                .display()
                .to_string(),
        };

        let handle = Arc::new(SubagentJobHandle::new(meta));
        {
            let mut jobs = self.inner.jobs.write().await;
            jobs.insert(unified_id.clone(), handle.clone());
        }

        let mut child_parent_ctx = parent_ctx.clone();
        if let Some(trace) = parent_ctx.trace.clone() {
            *handle.trace_context.lock().unwrap() = Some(trace.clone());
            let subagent_ctx = TraceContext {
                trace_id: trace.trace_id.clone(),
                run_id: trace.run_id.clone(),
                session_id: unified_id.clone(),
                root_session_id: trace.root_session_id.clone(),
                task_id: trace.task_id.clone(),
                turn_id: trace.turn_id.clone(),
                iteration: trace.iteration,
                parent_span_id: trace.parent_span_id.clone(),
            };
            let subagent_span = shared_bus().start_span(
                &subagent_ctx,
                TraceActor::Subagent,
                "subagent_spawned",
                serde_json::json!({
                    "job_id": unified_id,
                    "parent_session_id": parent_ctx.session_id,
                    "parent_reply_to": parent_ctx.reply_to,
                    "sub_session_id": handle.meta.sub_session_id,
                    "goal": handle.meta.goal,
                    "context": handle.meta.context,
                    "timeout_sec": execution.timeout_sec,
                    "max_steps": execution.max_steps,
                    "skill_name": handle.meta.skill_name,
                    "transcript_path": handle.meta.transcript_path,
                    "event_log_path": handle.meta.event_log_path,
                }),
            );
            let subagent_span_id = subagent_span.span_id().to_string();
            *handle.trace_span.lock().unwrap() = Some(subagent_span);
            if let Some(child_trace) = child_parent_ctx.trace.as_mut() {
                child_trace.parent_span_id = Some(subagent_span_id);
            }
        }

        let runtime = self.clone();
        let counter = self.inner.running_jobs.clone();
        let running_guard = RunningJobGuard::new(counter);
        let handle_for_task = handle.clone();
        let sub_session_id_for_task = unified_id.clone();
        let span = tracing::info_span!(
            "subagent_run",
            job_id = %unified_id,
            parent_session_id = %parent_ctx.session_id,
            sub_session_id = %sub_session_id_for_task
        );
        let join_handle = tokio::spawn(
            async move {
                let res = std::panic::AssertUnwindSafe(async {
                    let _guard = running_guard;
                    runtime
                        .run_job(
                            handle_for_task.clone(),
                            SubagentJobRequest {
                                parent_ctx: child_parent_ctx,
                                execution,
                                sub_session_id: sub_session_id_for_task,
                            },
                        )
                        .await;
                })
                .catch_unwind()
                .await;

                if let Err(err) = res {
                    let msg = if let Some(s) = err.downcast_ref::<&str>() {
                        s.to_string()
                    } else if let Some(s) = err.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "Unknown panic".to_string()
                    };
                    tracing::error!(target: "subagent", "[Sub:{}] Task panicked: {}", handle_for_task.meta.job_id, msg);
                    let mut state = handle_for_task.state.write().await;
                    if !state.is_terminal() {
                        *state = SubagentJobState::Failed {
                            finished_at_unix_ms: unix_ms_now(),
                            error: format!("Subagent panicked: {}", msg),
                            partial: None,
                        };
                    }
                    handle_for_task.completion_notify.notify_waiters();
                }
            }
            .instrument(span),
        );
        *handle.task.lock().await = Some(join_handle);

        Ok(SpawnedSubagentJob {
            job_id: unified_id.clone(),
            sub_session_id: unified_id,
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
        let debug = handle.debug.read().await.clone();
        Ok(SubagentJobSnapshot {
            meta: handle.meta.clone(),
            state,
            consumed: consumed_at_unix_ms.is_some(),
            consumed_at_unix_ms,
            debug,
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
                debug: handle.debug.read().await.clone(),
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
            handle.completion_notify.notify_waiters();
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

    pub(crate) async fn get_job_handle(&self, job_id: &str) -> Option<Arc<SubagentJobHandle>> {
        let jobs = self.inner.jobs.read().await;
        jobs.get(job_id).cloned()
    }

    async fn get_job_trace_context(
        &self,
        job_id: &str,
    ) -> Option<crate::tools::protocol::ToolTraceContext> {
        let handle = self.get_job_handle(job_id).await?;
        let trace = handle.trace_context.lock().unwrap().clone();
        trace
    }

    async fn run_job(&self, handle: Arc<SubagentJobHandle>, request: SubagentJobRequest) {
        {
            let mut state = handle.state.write().await;
            *state = SubagentJobState::Running {
                started_at_unix_ms: unix_ms_now(),
            };
        }
        self.set_debug_state_label(&handle, "running").await;

        let SubagentJobRequest {
            parent_ctx,
            execution,
            sub_session_id,
        } = request;

        if let Some(trace) = &parent_ctx.trace {
            shared_bus().record_event(
                &TraceContext {
                    trace_id: trace.trace_id.clone(),
                    run_id: trace.run_id.clone(),
                    session_id: handle.meta.sub_session_id.clone(),
                    root_session_id: trace.root_session_id.clone(),
                    task_id: trace.task_id.clone(),
                    turn_id: trace.turn_id.clone(),
                    iteration: trace.iteration,
                    parent_span_id: trace.parent_span_id.clone(),
                },
                TraceActor::Subagent,
                "subagent_state_changed",
                TraceStatus::Running,
                Some("running".to_string()),
                serde_json::json!({
                    "job_id": handle.meta.job_id,
                    "state": "running",
                }),
            );
        }

        let executed_state = match build_subagent_session(
            &parent_ctx,
            self.inner.llm.clone(),
            &self.inner.base_tools,
            crate::session::factory::SubagentSessionConfig {
                sub_session_id: Some(sub_session_id),
                allowed_tools: execution.allowed_tools.clone(),
                restrict_to_allowed_tools: execution.restrict_to_allowed_tools,
                energy_budget: execution.max_steps,
                timeout_sec: execution.timeout_sec,
                parent_context_text: execution.context.clone(),
                skill_session_seed: execution.skill_session_seed.clone(),
                debug: handle.debug.clone(),
                cancelled: handle.cancelled.clone(),
                cancel_notify: handle.cancel_notify.clone(),
                allow_subagent_tool: execution.allow_subagent_tool,
            },
        ) {
            Ok(BuiltSubagentSession {
                sub_session_id: _,
                transcript_path: _,
                event_log_path: _,
                mut agent_loop,
                collector,
            }) => {
                self.execute_subagent(handle.clone(), execution, collector, &mut agent_loop)
                    .await
            }
            Err(error) => {
                self.record_debug_error(&handle, "build_subagent_session", &error, None)
                    .await;
                SubagentJobState::Failed {
                    finished_at_unix_ms: unix_ms_now(),
                    error,
                    partial: None,
                }
            }
        };

        // Override state to Cancelled if the cancel flag was set during execution,
        // regardless of what execute_subagent returned.
        let final_state = if handle.cancelled.load(Ordering::SeqCst) {
            match executed_state {
                SubagentJobState::Cancelled { .. } => executed_state,
                SubagentJobState::Completed {
                    finished_at_unix_ms,
                    result,
                }
                | SubagentJobState::Failed {
                    finished_at_unix_ms,
                    partial: Some(result),
                    ..
                } => SubagentJobState::Cancelled {
                    finished_at_unix_ms,
                    partial: Some(result),
                },
                _ => SubagentJobState::Cancelled {
                    finished_at_unix_ms: unix_ms_now(),
                    partial: None,
                },
            }
        } else {
            executed_state
        };

        tracing::info!(
            target: "subagent",
            "[Sub:{}] Background execution finished with state: {}",
            handle.meta.job_id, final_state.finish_reason()
        );
        if let Some(span) = handle.trace_span.lock().unwrap().take() {
            let (status, summary) = match &final_state {
                SubagentJobState::Completed { result, .. } => {
                    (TraceStatus::Ok, Some(result.summary.clone()))
                }
                SubagentJobState::Failed { error, .. } => (TraceStatus::Error, Some(error.clone())),
                SubagentJobState::Cancelled { partial, .. } => (
                    TraceStatus::Cancelled,
                    partial.as_ref().map(|result| result.summary.clone()),
                ),
                SubagentJobState::TimedOut { partial, .. } => (
                    TraceStatus::TimedOut,
                    partial.as_ref().map(|result| result.summary.clone()),
                ),
                SubagentJobState::Pending | SubagentJobState::Running { .. } => {
                    (TraceStatus::Running, None)
                }
            };
            span.finish(
                "subagent_finished",
                status,
                summary,
                serde_json::json!({
                    "job_id": handle.meta.job_id,
                    "sub_session_id": handle.meta.sub_session_id,
                    "status": final_state.finish_reason(),
                    "transcript_path": handle.meta.transcript_path,
                    "event_log_path": handle.meta.event_log_path,
                }),
            );
        }
        self.enqueue_notification(&handle.meta, &final_state).await;
        // Only write state if cancel_job() hasn't already set a terminal state.
        let mut state = handle.state.write().await;
        if !state.is_terminal() {
            *state = final_state;
        }
        drop(state);
        self.finalize_debug_state(&handle).await;
        // Wake up any `subagent(action="status")` calls waiting via long polling.
        handle.completion_notify.notify_waiters();
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
        drop(notifications);
        if let Some(trace) = self.get_job_trace_context(&meta.job_id).await {
            shared_bus().record_event(
                &TraceContext {
                    trace_id: trace.trace_id.clone(),
                    run_id: trace.run_id.clone(),
                    session_id: meta.sub_session_id.clone(),
                    root_session_id: trace.root_session_id.clone(),
                    task_id: trace.task_id.clone(),
                    turn_id: trace.turn_id.clone(),
                    iteration: trace.iteration,
                    parent_span_id: trace.parent_span_id.clone(),
                },
                TraceActor::Subagent,
                "subagent_notification_enqueued",
                TraceStatus::Ok,
                Some(final_state.finish_reason().to_string()),
                serde_json::json!({
                    "job_id": meta.job_id,
                    "parent_session_id": meta.parent_session_id,
                    "sub_session_id": meta.sub_session_id,
                    "status": final_state.finish_reason(),
                }),
            );
        }
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
        execution: SubagentExecutionRequest,
        collector: Arc<crate::session::factory::CollectorOutput>,
        agent_loop: &mut crate::core::AgentLoop,
    ) -> SubagentJobState {
        let run_result = tokio::time::timeout(
            Duration::from_secs(execution.timeout_sec),
            agent_loop.step(execution.initial_input.clone()),
        )
        .await;

        // Give any in-flight async tool outputs an extra 50ms to flush to the collector
        // if we just timed out and cancelled their parent task.
        if run_result.is_err() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let collected_text = collector.take_text().await;
        let tool_outputs = collector.take_tool_outputs().await;
        let artifacts = collector.take_artifacts().await;
        let finished_at_unix_ms = unix_ms_now();
        let sub_session_id = Some(handle.meta.sub_session_id.clone());
        let transcript_path = Some(handle.meta.transcript_path.clone());
        let event_log_path = Some(handle.meta.event_log_path.clone());
        let skill_name = execution.skill_name.clone();
        let lineage = execution.lineage.clone();
        let effective_tools = execution.effective_tools.clone();
        let effective_max_steps = execution.effective_max_steps;
        let effective_timeout_sec = execution.effective_timeout_sec;

        match run_result {
            Ok(Ok(exit)) => {
                let ok = matches!(exit, crate::core::RunExit::Finished(_));
                let summary = match exit {
                    crate::core::RunExit::Finished(summary) => summary,
                    crate::core::RunExit::YieldedToUser => {
                        let message = if let Some(skill_name) = execution.skill_name.as_ref() {
                            format!(
                                "Delegated skill '{}' attempted to wait for user input, which is not allowed in subagents.",
                                skill_name
                            )
                        } else if collected_text.trim().is_empty() {
                            "Sub-agent yielded without visible output.".to_string()
                        } else {
                            format!("Sub-agent yielded with output: {}", collected_text.trim())
                        };
                        self.record_debug_error(&handle, "yielded_to_user", &message, None)
                            .await;
                        return SubagentJobState::Failed {
                            finished_at_unix_ms,
                            error: message.clone(),
                            partial: Some(SubagentResult {
                                ok: false,
                                summary: message,
                                findings: tool_outputs,
                                artifacts,
                                sub_session_id: sub_session_id.clone(),
                                transcript_path: transcript_path.clone(),
                                event_log_path: event_log_path.clone(),
                                skill_name,
                                lineage,
                                effective_tools,
                                effective_max_steps,
                                effective_timeout_sec,
                                failure: None,
                            }),
                        };
                    }
                    crate::core::RunExit::RecoverableFailed(message)
                    | crate::core::RunExit::CriticallyFailed(message)
                    | crate::core::RunExit::AutopilotStalled(message) => {
                        self.record_debug_error(&handle, "finish", &message, None)
                            .await;
                        return SubagentJobState::Failed {
                            finished_at_unix_ms,
                            error: message.clone(),
                            partial: Some(SubagentResult {
                                ok: false,
                                summary: message,
                                findings: tool_outputs,
                                artifacts,
                                sub_session_id: sub_session_id.clone(),
                                transcript_path: transcript_path.clone(),
                                event_log_path: event_log_path.clone(),
                                skill_name,
                                lineage,
                                effective_tools,
                                effective_max_steps,
                                effective_timeout_sec,
                                failure: None,
                            }),
                        };
                    }
                    crate::core::RunExit::EnergyDepleted(summary) => {
                        self.record_debug_error(
                            &handle,
                            "energy_depleted",
                            "Sub-agent ran out of energy.",
                            None,
                        )
                        .await;
                        return SubagentJobState::Failed {
                            finished_at_unix_ms,
                            error: "Sub-agent ran out of energy (iteration limit reached)."
                                .to_string(),
                            partial: Some(SubagentResult {
                                ok: false,
                                summary,
                                findings: tool_outputs,
                                artifacts,
                                sub_session_id: sub_session_id.clone(),
                                transcript_path: transcript_path.clone(),
                                event_log_path: event_log_path.clone(),
                                skill_name,
                                lineage,
                                effective_tools,
                                effective_max_steps,
                                effective_timeout_sec,
                                failure: None,
                            }),
                        };
                    }
                    crate::core::RunExit::StoppedByUser => {
                        self.record_debug_error(
                            &handle,
                            "cancelled",
                            "Sub-agent execution was interrupted.",
                            None,
                        )
                        .await;
                        return SubagentJobState::Cancelled {
                            finished_at_unix_ms,
                            partial: Some(SubagentResult {
                                ok: false,
                                summary: "Sub-agent execution was interrupted.".to_string(),
                                findings: tool_outputs,
                                artifacts,
                                sub_session_id: sub_session_id.clone(),
                                transcript_path: transcript_path.clone(),
                                event_log_path: event_log_path.clone(),
                                skill_name,
                                lineage,
                                effective_tools,
                                effective_max_steps,
                                effective_timeout_sec,
                                failure: None,
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
                        sub_session_id: sub_session_id.clone(),
                        transcript_path: transcript_path.clone(),
                        event_log_path: event_log_path.clone(),
                        skill_name,
                        lineage,
                        effective_tools,
                        effective_max_steps,
                        effective_timeout_sec,
                        failure: None,
                    },
                }
            }
            Ok(Err(error)) => {
                self.record_debug_error(&handle, "llm_stream_read", &error.to_string(), None)
                    .await;
                SubagentJobState::Failed {
                    finished_at_unix_ms,
                    error: error.to_string(),
                    partial: Some(SubagentResult {
                        ok: false,
                        summary: format!("Sub-agent error: {}", error),
                        findings: tool_outputs,
                        artifacts,
                        sub_session_id: sub_session_id.clone(),
                        transcript_path: transcript_path.clone(),
                        event_log_path: event_log_path.clone(),
                        skill_name,
                        lineage,
                        effective_tools,
                        effective_max_steps,
                        effective_timeout_sec,
                        failure: None,
                    }),
                }
            }
            Err(_) => {
                if handle.cancelled.load(Ordering::SeqCst) {
                    self.record_debug_error(
                        &handle,
                        "cancelled",
                        "Sub-agent execution was interrupted.",
                        None,
                    )
                    .await;
                    SubagentJobState::Cancelled {
                        finished_at_unix_ms,
                        partial: Some(SubagentResult {
                            ok: false,
                            summary: "Sub-agent execution was interrupted.".to_string(),
                            findings: tool_outputs,
                            artifacts,
                            sub_session_id: sub_session_id.clone(),
                            transcript_path: transcript_path.clone(),
                            event_log_path: event_log_path.clone(),
                            skill_name,
                            lineage,
                            effective_tools,
                            effective_max_steps,
                            effective_timeout_sec,
                            failure: None,
                        }),
                    }
                } else {
                    self.record_debug_error(
                        &handle,
                        "timeout",
                        &format!(
                            "Sub-agent timed out after {}s while working on '{}'.",
                            execution.timeout_sec, execution.display_goal
                        ),
                        None,
                    )
                    .await;
                    SubagentJobState::TimedOut {
                        finished_at_unix_ms,
                        partial: Some(SubagentResult {
                            ok: false,
                            summary: format!(
                                "Sub-agent timed out after {}s while working on '{}'.",
                                execution.timeout_sec, execution.display_goal
                            ),
                            findings: tool_outputs,
                            artifacts,
                            sub_session_id,
                            transcript_path,
                            event_log_path,
                            skill_name,
                            lineage,
                            effective_tools,
                            effective_max_steps,
                            effective_timeout_sec,
                            failure: None,
                        }),
                    }
                }
            }
        }
    }

    async fn set_debug_state_label(&self, handle: &SubagentJobHandle, label: &str) {
        let mut debug = handle.debug.write().await;
        debug.state_label = label.to_string();
        debug.updated_at_unix_ms = unix_ms_now();
    }

    async fn record_debug_error(
        &self,
        handle: &SubagentJobHandle,
        failure_stage: &str,
        error: &str,
        tool_name: Option<&str>,
    ) {
        let mut debug = handle.debug.write().await;
        debug.failure_stage = Some(failure_stage.to_string());
        debug.last_error = Some(truncate_debug_text(error, 500));
        if let Some(tool_name) = tool_name {
            debug.last_tool_name = Some(tool_name.to_string());
        }
        push_recent_debug_event(
            &mut debug,
            SubagentDebugEvent {
                kind: "error".to_string(),
                tool_name: tool_name.map(|value| value.to_string()),
                text: truncate_debug_text(error, 240),
                at_unix_ms: unix_ms_now(),
            },
        );
        debug.updated_at_unix_ms = unix_ms_now();
    }

    async fn finalize_debug_state(&self, handle: &SubagentJobHandle) {
        let state_label = {
            let state = handle.state.read().await;
            state.finish_reason().to_string()
        };
        let mut debug = handle.debug.write().await;
        debug.state_label = state_label;
        debug.updated_at_unix_ms = unix_ms_now();
        if debug.state_label == "finished" {
            debug.failure_stage = None;
        }
    }
}

pub(crate) fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

pub(crate) fn truncate_debug_text(input: &str, max_chars: usize) -> String {
    let truncated: String = input.chars().take(max_chars).collect();
    if input.chars().count() > max_chars {
        format!("{truncated}...(truncated)")
    } else {
        truncated
    }
}

pub(crate) fn push_recent_debug_event(
    debug: &mut SubagentDebugSnapshot,
    event: SubagentDebugEvent,
) {
    const MAX_RECENT_EVENTS: usize = 12;
    debug.recent_events.push(event);
    if debug.recent_events.len() > MAX_RECENT_EVENTS {
        let excess = debug.recent_events.len() - MAX_RECENT_EVENTS;
        debug.recent_events.drain(0..excess);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use serde_json::json;
    use tokio::sync::mpsc;

    use crate::context::{FunctionCall, Message};
    use crate::llm_client::{LlmCapabilities, LlmError, StreamEvent};

    struct FinishImmediatelyLlm;

    #[async_trait]
    impl LlmClient for FinishImmediatelyLlm {
        fn model_name(&self) -> &str {
            "finish-immediately"
        }

        fn provider_name(&self) -> &str {
            "test"
        }

        fn capabilities(&self) -> LlmCapabilities {
            LlmCapabilities {
                function_tools: true,
                custom_tools: false,
                parallel_tool_calls: true,
                supports_code_mode: true,
            }
        }

        async fn stream(
            &self,
            _messages: Vec<Message>,
            _system_instruction: Option<Message>,
            _tools: Vec<Arc<dyn Tool>>,
        ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
            let (tx, rx) = mpsc::channel(4);
            let _ = tx.try_send(StreamEvent::ToolCall(
                FunctionCall {
                    name: "finish_task".to_string(),
                    args: json!({ "summary": "done" }),
                    id: Some("tc_1".to_string()),
                },
                None,
            ));
            let _ = tx.try_send(StreamEvent::Done);
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

        fn capabilities(&self) -> LlmCapabilities {
            LlmCapabilities {
                function_tools: true,
                custom_tools: false,
                parallel_tool_calls: true,
                supports_code_mode: true,
            }
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
        ToolContext::new("parent", "cli")
    }

    fn make_context() -> String {
        "summary".to_string()
    }

    fn make_meta(job_id: &str, sub_session_id: &str) -> SubagentJobMeta {
        SubagentJobMeta {
            job_id: job_id.to_string(),
            parent_session_id: "parent".to_string(),
            parent_reply_to: "cli".to_string(),
            sub_session_id: sub_session_id.to_string(),
            goal: "inspect".to_string(),
            context: "summary".to_string(),
            skill_name: None,
            created_at_unix_ms: unix_ms_now(),
            transcript_path: crate::schema::StoragePaths::session_transcript_file(sub_session_id)
                .display()
                .to_string(),
            event_log_path: crate::schema::StoragePaths::events_file(sub_session_id)
                .display()
                .to_string(),
        }
    }

    async fn wait_for_terminal_state(runtime: &SubagentRuntime, job_id: &str) -> SubagentJobState {
        for _ in 0..400 {
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
            .spawn_job(make_ctx(), "inspect".to_string(), make_context())
            .await
            .unwrap();

        let state = wait_for_terminal_state(&runtime, &spawned.job_id).await;
        match state {
            SubagentJobState::Completed { result, .. } => {
                assert!(result.ok);
                assert!(result.summary.contains("done"));
                let snapshot = runtime
                    .get_job_snapshot(&spawned.job_id, false)
                    .await
                    .unwrap();
                assert_eq!(snapshot.meta.context, "summary");
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
            .spawn_job(make_ctx(), "hang".to_string(), make_context())
            .await
            .unwrap();

        let second = runtime
            .spawn_job(make_ctx(), "blocked".to_string(), make_context())
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
            .spawn_job(make_ctx(), "hang".to_string(), make_context())
            .await
            .unwrap();

        runtime.cancel_job(&spawned.job_id).await.unwrap();
        let state = wait_for_terminal_state(&runtime, &spawned.job_id).await;
        assert!(matches!(state, SubagentJobState::Cancelled { .. }));
    }

    #[tokio::test]
    async fn test_get_job_snapshot_can_mark_terminal_job_consumed() {
        let runtime = SubagentRuntime::new(
            Arc::new(FinishImmediatelyLlm),
            vec![Arc::new(MockTool("read_file"))],
            2,
        );

        let spawned = runtime
            .spawn_job(make_ctx(), "inspect".to_string(), make_context())
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

        let consumed_job = Arc::new(SubagentJobHandle::new(make_meta(
            "consumed",
            "sub_consumed",
        )));
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
                    sub_session_id: Some("sub_consumed".to_string()),
                    transcript_path: Some(
                        crate::schema::StoragePaths::session_transcript_file("sub_consumed")
                            .display()
                            .to_string(),
                    ),
                    event_log_path: Some(
                        crate::schema::StoragePaths::events_file("sub_consumed")
                            .display()
                            .to_string(),
                    ),
                    skill_name: None,
                    lineage: None,
                    effective_tools: None,
                    effective_max_steps: None,
                    effective_timeout_sec: None,
                    failure: None,
                },
            };
        }
        {
            let mut consumed_at = consumed_job.consumed_at_unix_ms.write().await;
            *consumed_at = Some(unix_ms_now());
        }

        let unconsumed_job = Arc::new(SubagentJobHandle::new(make_meta(
            "unconsumed",
            "sub_unconsumed",
        )));
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
                    sub_session_id: Some("sub_unconsumed".to_string()),
                    transcript_path: Some(
                        crate::schema::StoragePaths::session_transcript_file("sub_unconsumed")
                            .display()
                            .to_string(),
                    ),
                    event_log_path: Some(
                        crate::schema::StoragePaths::events_file("sub_unconsumed")
                            .display()
                            .to_string(),
                    ),
                    skill_name: None,
                    lineage: None,
                    effective_tools: None,
                    effective_max_steps: None,
                    effective_timeout_sec: None,
                    failure: None,
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
    async fn test_job_snapshot_includes_debug_details_and_artifact_paths() {
        let runtime = SubagentRuntime::new(
            Arc::new(FinishImmediatelyLlm),
            vec![Arc::new(MockTool("read_file"))],
            2,
        );

        let spawned = runtime
            .spawn_job(make_ctx(), "inspect cargo".to_string(), make_context())
            .await
            .unwrap();

        let _ = wait_for_terminal_state(&runtime, &spawned.job_id).await;
        let snapshot = runtime
            .get_job_snapshot(&spawned.job_id, false)
            .await
            .unwrap();

        assert_eq!(snapshot.debug.state_label, "finished");
        assert_eq!(
            snapshot.debug.last_tool_name.as_deref(),
            Some("finish_task")
        );
        assert!(snapshot
            .debug
            .recent_events
            .iter()
            .any(|event| event.kind == "subagent_tool_start"));
        assert!(snapshot
            .debug
            .recent_events
            .iter()
            .any(|event| event.kind == "subagent_tool_end"));
        assert!(std::path::Path::new(&snapshot.meta.transcript_path).exists());
        assert!(std::path::Path::new(&snapshot.meta.event_log_path).exists());
    }

    #[tokio::test]
    async fn test_timeout_snapshot_includes_failure_stage() {
        let runtime = SubagentRuntime::new(
            Arc::new(HangingLlm),
            vec![Arc::new(MockTool("read_file"))],
            1,
        );

        let spawned = runtime
            .spawn_job_with_limits(
                make_ctx(),
                SubagentExecutionRequest {
                    initial_input: "hang".to_string(),
                    display_goal: "hang".to_string(),
                    context: make_context(),
                    timeout_sec: 1,
                    max_steps: 4,
                    allowed_tools: Vec::new(),
                    restrict_to_allowed_tools: false,
                    allow_subagent_tool: false,
                    skill_name: None,
                    lineage: None,
                    effective_tools: None,
                    effective_max_steps: Some(4),
                    effective_timeout_sec: Some(1),
                    skill_session_seed: SkillSessionSeed::default(),
                },
            )
            .await
            .unwrap();

        let state = wait_for_terminal_state(&runtime, &spawned.job_id).await;
        assert!(matches!(state, SubagentJobState::TimedOut { .. }));

        let snapshot = runtime
            .get_job_snapshot(&spawned.job_id, false)
            .await
            .unwrap();
        assert_eq!(snapshot.debug.state_label, "timed_out");
        assert_eq!(snapshot.debug.failure_stage.as_deref(), Some("timeout"));
        let partial = match snapshot.state {
            SubagentJobState::TimedOut { partial, .. } => partial.expect("partial timeout result"),
            other => panic!("expected timed out state, got {:?}", other),
        };
        assert!(partial.summary.contains("timed out after 1s"));
    }
}
