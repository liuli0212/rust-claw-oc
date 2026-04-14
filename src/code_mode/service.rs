use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::cell::{ActiveCellHandle, CellDrainSnapshot, CellStatus};
use super::driver::{CellDriver, DriverCompletion, DriverDrainBatch};
use super::protocol::DrainRequest;
use super::response::ExecRunResult;
use super::runtime;

#[derive(Debug, Default, Clone)]
pub struct CodeModeService {
    sessions: Arc<Mutex<HashMap<String, SessionState>>>,
}

type SharedCellDriver = Arc<Mutex<CellDriver>>;

#[derive(Debug, Default)]
struct SessionState {
    next_cell_seq: u64,
    stored_values: HashMap<String, serde_json::Value>,
    active_cell: Option<ActiveCellHandle>,
    live_driver: Option<SharedCellDriver>,
}

#[derive(Debug, Clone)]
struct PendingCellContext {
    active_cell: ActiveCellHandle,
    drain_snapshot: CellDrainSnapshot,
    live_driver: SharedCellDriver,
}

#[derive(Debug)]
struct PendingDrainBatch {
    active_cell: ActiveCellHandle,
    prior_snapshot: CellDrainSnapshot,
    batch: DriverDrainBatch,
}

enum PendingDrainResolution {
    Progress {
        active_cell: ActiveCellHandle,
        prior_snapshot: CellDrainSnapshot,
        batch: Box<DriverDrainBatch>,
    },
    Completion {
        active_cell: ActiveCellHandle,
        completion: Box<DriverCompletion>,
    },
}

impl PendingDrainBatch {
    #[cfg(test)]
    fn should_fallback_to_prior_snapshot(&self) -> bool {
        self.batch.requested_wait_for_event() && self.batch.is_empty()
    }

    fn into_resolution(self) -> Result<PendingDrainResolution, crate::tools::ToolError> {
        let PendingDrainBatch {
            active_cell,
            prior_snapshot,
            batch,
        } = self;

        if batch.terminal_result.is_some() {
            return Ok(PendingDrainResolution::Completion {
                active_cell,
                completion: Box::new(batch.into_completion()?),
            });
        }

        Ok(PendingDrainResolution::Progress {
            active_cell,
            prior_snapshot,
            batch: Box::new(batch),
        })
    }
}

struct RuntimeBatchInvocation {
    cell_id: String,
    code: String,
    visible_tools: Vec<String>,
    stored_values: HashMap<String, serde_json::Value>,
    resume_state: runtime::ResumeState,
    request: DrainRequest,
}

impl RuntimeBatchInvocation {
    fn for_execute(
        cell_id: String,
        code: String,
        visible_tools: Vec<String>,
        stored_values: HashMap<String, serde_json::Value>,
    ) -> Self {
        Self::for_execute_with_request(
            cell_id,
            code,
            visible_tools,
            stored_values,
            DrainRequest::to_completion(),
        )
    }

    fn for_execute_with_request(
        cell_id: String,
        code: String,
        visible_tools: Vec<String>,
        stored_values: HashMap<String, serde_json::Value>,
        request: DrainRequest,
    ) -> Self {
        Self {
            cell_id,
            code,
            visible_tools,
            stored_values,
            resume_state: runtime::ResumeState::default(),
            request,
        }
    }

    #[cfg(test)]
    fn for_pending_cell(
        active_cell: &ActiveCellHandle,
        stored_values: HashMap<String, serde_json::Value>,
        request: DrainRequest,
    ) -> Self {
        Self {
            cell_id: active_cell.cell_id.clone(),
            code: active_cell.code.clone(),
            visible_tools: active_cell.visible_tools.clone(),
            stored_values,
            resume_state: active_cell.runtime_resume_state(),
            request,
        }
    }
}

impl CodeModeService {
    pub async fn execute<F, Fut>(
        &self,
        session_id: &str,
        code: &str,
        visible_tools: Vec<String>,
        invoke_tool: &mut F,
    ) -> Result<ExecRunResult, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        let (cell_id, stored_values) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions.entry(session_id.to_string()).or_default();
            if let Some(active_cell) = session.active_cell.as_ref() {
                return Err(crate::tools::ToolError::ExecutionFailed(format!(
                    "A pending code mode cell already exists for this session (`{}`). Use `wait` before starting a new `exec`.",
                    active_cell.cell_id
                )));
            }
            session.next_cell_seq += 1;
            (
                format!("cell_{}", session.next_cell_seq),
                session.stored_values.clone(),
            )
        };

        let completion = self
            .run_runtime_cell_batch_with_request(
                RuntimeBatchInvocation::for_execute(
                    cell_id.clone(),
                    code.to_string(),
                    visible_tools.clone(),
                    stored_values,
                ),
                invoke_tool,
            )
            .await?
            .into_completion()?;

        self.apply_execute_completion(
            session_id,
            cell_id,
            code.to_string(),
            visible_tools,
            completion,
        )
        .await
    }

    async fn pending_cell_context(
        &self,
        session_id: &str,
        requested_cell_id: Option<&str>,
    ) -> Result<PendingCellContext, crate::tools::ToolError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions.get_mut(session_id).ok_or_else(|| {
            crate::tools::ToolError::ExecutionFailed(
                "No pending code mode cell is available for this session.".to_string(),
            )
        })?;
        let active_cell = session.active_cell.clone().ok_or_else(|| {
            crate::tools::ToolError::ExecutionFailed(
                "No pending code mode cell is available for this session.".to_string(),
            )
        })?;

        if let Some(cell_id) = requested_cell_id {
            if cell_id != active_cell.cell_id {
                return Err(crate::tools::ToolError::ExecutionFailed(format!(
                    "Pending code mode cell mismatch: expected `{}`, got `{}`.",
                    active_cell.cell_id, cell_id
                )));
            }
        }

        Ok(PendingCellContext {
            drain_snapshot: active_cell.drain_snapshot(),
            live_driver: match session.live_driver.clone() {
                Some(live_driver) => live_driver,
                None => {
                    let live_driver = Arc::new(Mutex::new(CellDriver::spawn_live(
                        active_cell.cell_id.clone(),
                        active_cell.code.clone(),
                        active_cell.visible_tools.clone(),
                        session.stored_values.clone(),
                        active_cell.runtime_resume_state(),
                        matches!(active_cell.status, CellStatus::WaitingOnJsTimer { .. }),
                    )));
                    session.live_driver = Some(live_driver.clone());
                    live_driver
                }
            },
            active_cell,
        })
    }

    pub async fn wait<F, Fut>(
        &self,
        session_id: &str,
        requested_cell_id: Option<&str>,
        invoke_tool: &mut F,
    ) -> Result<ExecRunResult, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        self.wait_with_request(
            session_id,
            requested_cell_id,
            DrainRequest::wait_for_next_event(),
            invoke_tool,
        )
        .await
    }

    pub async fn poll<F, Fut>(
        &self,
        session_id: &str,
        requested_cell_id: Option<&str>,
        invoke_tool: &mut F,
    ) -> Result<ExecRunResult, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        self.wait_with_request(
            session_id,
            requested_cell_id,
            DrainRequest::poll_now(),
            invoke_tool,
        )
        .await
    }

    pub(crate) async fn wait_with_request<F, Fut>(
        &self,
        session_id: &str,
        requested_cell_id: Option<&str>,
        request: DrainRequest,
        invoke_tool: &mut F,
    ) -> Result<ExecRunResult, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        let context = self
            .pending_cell_context(session_id, requested_cell_id)
            .await?;

        match self
            .drain_pending_cell_context(context, request, invoke_tool)
            .await
        {
            Ok(pending) => match pending.into_resolution() {
                Ok(resolution) => {
                    self.apply_pending_wait_resolution(session_id, resolution)
                        .await
                }
                Err(err) => {
                    self.clear_active_cell(session_id).await;
                    Err(err)
                }
            },
            Err(err) => {
                self.clear_active_cell(session_id).await;
                Err(err)
            }
        }
    }

    #[cfg(test)]
    async fn run_pending_cell_batch<F, Fut>(
        &self,
        session_id: &str,
        requested_cell_id: Option<&str>,
        request: DrainRequest,
        invoke_tool: &mut F,
    ) -> Result<PendingDrainBatch, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        let context = self
            .pending_cell_context(session_id, requested_cell_id)
            .await?;
        self.drain_pending_cell_context(context, request, invoke_tool)
            .await
    }

    async fn drain_pending_cell_context<F, Fut>(
        &self,
        context: PendingCellContext,
        request: DrainRequest,
        invoke_tool: &mut F,
    ) -> Result<PendingDrainBatch, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        let PendingCellContext {
            active_cell,
            drain_snapshot,
            live_driver,
        } = context;

        let batch = {
            let mut live_driver = live_driver.lock().await;
            live_driver
                .drain_event_batch_with_request(request, invoke_tool)
                .await?
        };

        Ok(PendingDrainBatch {
            active_cell,
            prior_snapshot: drain_snapshot,
            batch,
        })
    }

    async fn run_runtime_cell_batch_with_request<F, Fut>(
        &self,
        invocation: RuntimeBatchInvocation,
        invoke_tool: &mut F,
    ) -> Result<DriverDrainBatch, crate::tools::ToolError>
    where
        F: FnMut(String, String) -> Fut,
        Fut: Future<Output = Result<String, crate::tools::ToolError>>,
    {
        let RuntimeBatchInvocation {
            cell_id,
            code,
            visible_tools,
            stored_values,
            resume_state,
            request,
        } = invocation;

        let mut driver =
            CellDriver::spawn(cell_id, code, visible_tools, stored_values, resume_state);
        driver
            .drain_event_batch_with_request(request, invoke_tool)
            .await
    }

    async fn apply_terminal_wait_completion(
        &self,
        session_id: &str,
        active_cell: ActiveCellHandle,
        completion: DriverCompletion,
    ) -> Result<ExecRunResult, crate::tools::ToolError> {
        let DriverCompletion {
            runtime_result,
            events,
        } = completion;
        let last_event_seq =
            super::protocol::max_event_seq(&events).max(active_cell.last_event_seq);
        let (mut summary, stored_values, metadata) = runtime_result;
        let current_turn_nested_tool_calls = summary.nested_tool_calls;
        summary.nested_tool_calls =
            active_cell.drain_snapshot().nested_tool_calls + current_turn_nested_tool_calls;

        let next_active_cell = if summary.yielded {
            Some(active_cell.advance_with_yield(
                current_turn_nested_tool_calls,
                &summary,
                &metadata,
                events,
                last_event_seq,
            ))
        } else {
            None
        };
        let live_driver = next_active_cell
            .as_ref()
            .map(|next_active_cell| self.spawn_live_driver(next_active_cell, &stored_values));

        self.persist_session_state(session_id, stored_values, next_active_cell, live_driver)
            .await;

        Ok(summary)
    }

    async fn apply_in_progress_wait_batch(
        &self,
        session_id: &str,
        active_cell: ActiveCellHandle,
        prior_snapshot: CellDrainSnapshot,
        batch: DriverDrainBatch,
    ) -> Result<ExecRunResult, crate::tools::ToolError> {
        let should_fallback_to_prior_snapshot =
            batch.requested_wait_for_event() && batch.is_empty();
        let DriverDrainBatch {
            request: _,
            terminal_result: _,
            events,
            resume_progress,
        } = batch;
        let last_event_seq =
            super::protocol::max_event_seq(&events).max(active_cell.last_event_seq);
        let active_cell = active_cell.advance_with_events(events, resume_progress, last_event_seq);
        let result = if should_fallback_to_prior_snapshot {
            prior_snapshot.to_exec_result(active_cell.cell_id.clone())
        } else {
            active_cell
                .drain_snapshot()
                .to_exec_result(active_cell.cell_id.clone())
        };
        self.persist_active_cell(session_id, Some(active_cell))
            .await;
        Ok(result)
    }

    async fn apply_pending_wait_resolution(
        &self,
        session_id: &str,
        resolution: PendingDrainResolution,
    ) -> Result<ExecRunResult, crate::tools::ToolError> {
        match resolution {
            PendingDrainResolution::Progress {
                active_cell,
                prior_snapshot,
                batch,
            } => {
                self.apply_in_progress_wait_batch(session_id, active_cell, prior_snapshot, *batch)
                    .await
            }
            PendingDrainResolution::Completion {
                active_cell,
                completion,
            } => {
                self.apply_terminal_wait_completion(session_id, active_cell, *completion)
                    .await
            }
        }
    }

    async fn apply_execute_completion(
        &self,
        session_id: &str,
        cell_id: String,
        code: String,
        visible_tools: Vec<String>,
        completion: DriverCompletion,
    ) -> Result<ExecRunResult, crate::tools::ToolError> {
        let DriverCompletion {
            runtime_result,
            events,
        } = completion;
        let last_event_seq = super::protocol::max_event_seq(&events);
        let (mut summary, stored_values, metadata) = runtime_result;
        let next_active_cell = if summary.yielded {
            Some(ActiveCellHandle::from_initial_yield(
                cell_id.clone(),
                code,
                visible_tools,
                &summary,
                &metadata,
                events,
                last_event_seq,
            ))
        } else {
            None
        };
        let live_driver = next_active_cell
            .as_ref()
            .map(|next_active_cell| self.spawn_live_driver(next_active_cell, &stored_values));
        self.persist_session_state(session_id, stored_values, next_active_cell, live_driver)
            .await;
        summary.cell_id = cell_id;
        Ok(summary)
    }

    fn spawn_live_driver(
        &self,
        active_cell: &ActiveCellHandle,
        stored_values: &HashMap<String, serde_json::Value>,
    ) -> SharedCellDriver {
        Arc::new(Mutex::new(CellDriver::spawn_live(
            active_cell.cell_id.clone(),
            active_cell.code.clone(),
            active_cell.visible_tools.clone(),
            stored_values.clone(),
            active_cell.runtime_resume_state(),
            matches!(active_cell.status, CellStatus::WaitingOnJsTimer { .. }),
        )))
    }

    async fn persist_session_state(
        &self,
        session_id: &str,
        stored_values: HashMap<String, serde_json::Value>,
        active_cell: Option<ActiveCellHandle>,
        live_driver: Option<SharedCellDriver>,
    ) {
        let mut sessions = self.sessions.lock().await;
        let session = sessions.entry(session_id.to_string()).or_default();
        session.stored_values = stored_values;
        session.active_cell = active_cell;
        session.live_driver = live_driver;
    }

    async fn persist_active_cell(&self, session_id: &str, active_cell: Option<ActiveCellHandle>) {
        let mut sessions = self.sessions.lock().await;
        let session = sessions.entry(session_id.to_string()).or_default();
        session.active_cell = active_cell;
    }

    async fn clear_active_cell(&self, session_id: &str) {
        let mut sessions = self.sessions.lock().await;
        let session = sessions.entry(session_id.to_string()).or_default();
        session.active_cell = None;
        session.live_driver = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_mode::cell::{ActiveCellHandle, CellDrainSnapshot, CellResumeProgressDelta};
    use crate::code_mode::response::ExecYieldKind;
    use std::sync::{Arc as StdArc, Mutex as StdMutex};
    use std::time::Duration;

    #[tokio::test]
    async fn test_service_persists_values_across_cells() {
        let service = CodeModeService::default();
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        let first = service
            .execute(
                "session-a",
                r#"
store("answer", { value: 42 });
text("stored");
"#,
                Vec::new(),
                &mut invoke_tool,
            )
            .await
            .expect("first exec succeeds");
        assert!(first.output_text.contains("stored"));

        let second = service
            .execute(
                "session-a",
                r#"
const value = load("answer");
text(String(value.value));
"#,
                Vec::new(),
                &mut invoke_tool,
            )
            .await
            .expect("second exec succeeds");

        assert!(second.output_text.contains("42"));
    }

    #[tokio::test]
    async fn test_service_runs_nested_tool_calls() {
        let service = CodeModeService::default();
        let mut invoke_tool = |tool: String, args_json: String| async move {
            if tool == "echo_tool" {
                Ok(serde_json::json!({
                    "tool": tool,
                    "args": serde_json::from_str::<serde_json::Value>(&args_json).unwrap(),
                })
                .to_string())
            } else {
                Err(crate::tools::ToolError::ExecutionFailed(
                    "unexpected tool".to_string(),
                ))
            }
        };

        let result = service
            .execute(
                "session-b",
                r#"
const response = await tools.echo_tool({ value: "hello" });
text(response.args.value);
"#,
                vec!["echo_tool".to_string()],
                &mut invoke_tool,
            )
            .await
            .expect("exec succeeds");

        assert_eq!(result.nested_tool_calls, 1);
        assert!(result.output_text.contains("hello"));
    }

    #[tokio::test]
    async fn test_service_wait_resumes_yielded_cell_without_repeating_tool_calls() {
        let service = CodeModeService::default();
        let mut invoke_tool = move |tool: String, args_json: String| {
            let calls = StdArc::new(StdMutex::new(Vec::<String>::new()));
            let calls_for_tool = calls.clone();
            async move {
                calls_for_tool
                    .lock()
                    .unwrap()
                    .push(format!("{}:{}", tool, args_json));
                let value = serde_json::from_str::<serde_json::Value>(&args_json)
                    .unwrap()
                    .get("value")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                Ok(serde_json::json!({ "value": value }).to_string())
            }
        };

        let first = service
            .execute(
                "session-c",
                r#"
const first = await tools.echo_tool({ value: "hello" });
text(first.value);
yield_control("pause");
const second = await tools.echo_tool({ value: "world" });
text(second.value);
"#,
                vec!["echo_tool".to_string()],
                &mut invoke_tool,
            )
            .await
            .expect("first exec yields");

        assert!(first.yielded);
        assert_eq!(first.yield_kind, Some(ExecYieldKind::Manual));
        assert_eq!(first.yield_value, Some(serde_json::json!("pause")));
        assert_eq!(first.nested_tool_calls, 1);
        assert_eq!(first.output_text.trim(), "hello");

        let resumed = service
            .wait("session-c", Some(&first.cell_id), &mut invoke_tool)
            .await
            .expect("wait resumes pending cell");

        assert!(!resumed.yielded);
        assert_eq!(resumed.nested_tool_calls, 2);
        assert_eq!(resumed.output_text.trim(), "world");
    }

    #[tokio::test]
    async fn test_service_wait_with_request_resumes_pending_cell() {
        let service = CodeModeService::default();
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        let first = service
            .execute(
                "session-c-helper",
                r#"
text("before");
yield_control("pause");
text("after");
"#,
                Vec::new(),
                &mut invoke_tool,
            )
            .await
            .expect("first exec yields");

        let resumed = service
            .wait_with_request(
                "session-c-helper",
                Some(&first.cell_id),
                DrainRequest::wait_for_next_event(),
                &mut invoke_tool,
            )
            .await
            .expect("request-aware wait helper resumes pending cell");

        assert!(!resumed.yielded);
        assert_eq!(resumed.output_text.trim(), "after");
    }

    #[tokio::test]
    async fn test_service_wait_with_timeout_returns_current_snapshot_without_new_visible_events() {
        let service = CodeModeService::default();
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        let first = service
            .execute(
                "session-c-timeout",
                r#"
yield_control("resume");
const target = Date.now() + 40;
while (Date.now() < target) {}
text("after");
"#,
                Vec::new(),
                &mut invoke_tool,
            )
            .await
            .expect("first exec yields");

        let timed_out = service
            .wait_with_request(
                "session-c-timeout",
                Some(&first.cell_id),
                DrainRequest::for_wait(Some(5), None),
                &mut invoke_tool,
            )
            .await
            .expect("timed wait returns the current snapshot");

        assert!(timed_out.yielded);
        assert_eq!(timed_out.yield_value, Some(serde_json::json!("resume")));
        assert!(timed_out.output_text.trim().is_empty());

        let resumed = service
            .wait("session-c-timeout", Some(&first.cell_id), &mut invoke_tool)
            .await
            .expect("subsequent wait resumes the same live worker");

        assert!(!resumed.yielded);
        assert_eq!(resumed.output_text.trim(), "after");
    }

    #[tokio::test]
    async fn test_service_execute_rejects_when_pending_cell_exists() {
        let service = CodeModeService::default();
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        let first = service
            .execute(
                "session-exec-reject",
                r#"
yield_control("pause");
"#,
                Vec::new(),
                &mut invoke_tool,
            )
            .await
            .expect("first exec yields");

        let err = service
            .execute(
                "session-exec-reject",
                r#"
text("second");
"#,
                Vec::new(),
                &mut invoke_tool,
            )
            .await
            .expect_err("second exec is rejected while a cell is pending");

        assert!(err
            .to_string()
            .contains("A pending code mode cell already exists"));

        let sessions = service.sessions.lock().await;
        let active_cell = sessions
            .get("session-exec-reject")
            .and_then(|session| session.active_cell.as_ref())
            .expect("pending cell remains after rejected exec");
        assert_eq!(active_cell.cell_id, first.cell_id);
    }

    #[tokio::test]
    async fn test_service_wait_mismatched_cell_id_preserves_active_cell() {
        let service = CodeModeService::default();
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        let first = service
            .execute(
                "session-wait-mismatch",
                r#"
yield_control("pause");
"#,
                Vec::new(),
                &mut invoke_tool,
            )
            .await
            .expect("initial exec yields");

        let err = service
            .wait(
                "session-wait-mismatch",
                Some("cell_other"),
                &mut invoke_tool,
            )
            .await
            .expect_err("mismatched cell id is rejected");

        assert!(err.to_string().contains("Pending code mode cell mismatch"));

        let sessions = service.sessions.lock().await;
        let active_cell = sessions
            .get("session-wait-mismatch")
            .and_then(|session| session.active_cell.as_ref())
            .expect("active cell remains after mismatch");
        assert_eq!(active_cell.cell_id, first.cell_id);
    }

    #[tokio::test]
    async fn test_service_poll_mismatched_cell_id_preserves_active_cell() {
        let service = CodeModeService::default();
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        let first = service
            .execute(
                "session-poll-mismatch",
                r#"
yield_control("pause");
"#,
                Vec::new(),
                &mut invoke_tool,
            )
            .await
            .expect("initial exec yields");

        let err = service
            .poll(
                "session-poll-mismatch",
                Some("cell_other"),
                &mut invoke_tool,
            )
            .await
            .expect_err("mismatched cell id is rejected");

        assert!(err.to_string().contains("Pending code mode cell mismatch"));

        let sessions = service.sessions.lock().await;
        let active_cell = sessions
            .get("session-poll-mismatch")
            .and_then(|session| session.active_cell.as_ref())
            .expect("active cell remains after mismatch");
        assert_eq!(active_cell.cell_id, first.cell_id);
    }

    #[tokio::test]
    async fn test_service_wait_resumes_timer_driven_cells_with_incremental_output() {
        let service = CodeModeService::default();
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        let first = service
            .execute(
                "session-d",
                r#"
text("before");
setTimeout(async () => {
  text("later");
}, 20);
text("after");
"#,
                Vec::new(),
                &mut invoke_tool,
            )
            .await
            .expect("timer-based exec yields");

        assert!(first.yielded);
        assert_eq!(first.yield_kind, Some(ExecYieldKind::Timer));
        assert_eq!(first.output_text.trim(), "before\nafter");

        tokio::time::sleep(Duration::from_millis(30)).await;

        let resumed = service
            .wait("session-d", Some(&first.cell_id), &mut invoke_tool)
            .await
            .expect("timer-based wait resumes pending cell");

        assert!(!resumed.yielded);
        assert_eq!(resumed.output_text.trim(), "later");
    }

    #[tokio::test]
    async fn test_service_bounds_recent_events_for_yielded_cells() {
        let service = CodeModeService::default();
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        let result = service
            .execute(
                "session-e",
                r#"
text("x".repeat(9000));
yield_control("pause");
"#,
                Vec::new(),
                &mut invoke_tool,
            )
            .await
            .expect("exec yields with large output");

        assert!(result.yielded);

        let sessions = service.sessions.lock().await;
        let active_cell = sessions
            .get("session-e")
            .and_then(|session| session.active_cell.as_ref())
            .expect("active cell is retained after yield");

        assert!(active_cell.recent_events_truncated);
        assert_eq!(active_cell.recent_events.len(), 1);
        assert!(matches!(
            active_cell.recent_events.first(),
            Some(super::super::protocol::RuntimeEvent::Yield {
                seq: 2,
                kind: crate::code_mode::response::ExecYieldKind::Manual,
                ..
            })
        ));
        assert!(active_cell
            .render_recent_events(false)
            .contains("[output truncated to stay within the code-mode budget]"));
    }

    #[tokio::test]
    async fn test_service_exposes_pending_cell_drain_snapshot() {
        let service = CodeModeService::default();
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        let result = service
            .execute(
                "session-f",
                r#"
text("x".repeat(9000));
yield_control("pause");
"#,
                Vec::new(),
                &mut invoke_tool,
            )
            .await
            .expect("exec yields with large output");

        let pending = service
            .pending_cell_context("session-f", Some(&result.cell_id))
            .await
            .expect("pending cell context exists");

        assert!(pending.drain_snapshot.truncated);
        assert_eq!(pending.drain_snapshot.nested_tool_calls, 0);
        assert_eq!(
            pending.drain_snapshot.render_state.yield_kind,
            Some(ExecYieldKind::Manual)
        );
        assert!(pending
            .drain_snapshot
            .render(&result.cell_id)
            .contains("[output truncated to stay within the code-mode budget]"));
    }

    #[tokio::test]
    async fn test_service_run_pending_cell_batch_preserves_prior_snapshot_and_request() {
        let service = CodeModeService::default();
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        let first = service
            .execute(
                "session-g",
                r#"
text("before");
yield_control("pause");
text("after");
"#,
                Vec::new(),
                &mut invoke_tool,
            )
            .await
            .expect("exec yields");

        let pending = service
            .run_pending_cell_batch(
                "session-g",
                Some(&first.cell_id),
                DrainRequest::wait_for_next_event(),
                &mut invoke_tool,
            )
            .await
            .expect("pending drain batch exists");

        assert_eq!(pending.active_cell.cell_id, first.cell_id);
        assert_eq!(
            pending.prior_snapshot.status,
            crate::code_mode::cell::CellStatus::Running
        );
        assert_eq!(
            pending.prior_snapshot.render_state.yield_kind,
            Some(ExecYieldKind::Manual)
        );
        assert_eq!(pending.batch.request, DrainRequest::wait_for_next_event());
        assert!(matches!(
            pending.batch.events.last(),
            Some(super::super::protocol::RuntimeEvent::Completed { .. })
        ));

        let resolution = pending.into_resolution().expect("resolution succeeds");
        assert!(matches!(
            resolution,
            PendingDrainResolution::Completion { completion, .. }
                if completion.events.last().is_some()
        ));
    }

    #[test]
    fn test_pending_drain_batch_resolves_non_terminal_progress() {
        let pending = PendingDrainBatch {
            active_cell: ActiveCellHandle {
                cell_id: "cell_progress_1".to_string(),
                code: "tool_call()".to_string(),
                visible_tools: vec!["read_file".to_string()],
                status: crate::code_mode::cell::CellStatus::Running,
                last_event_seq: 1,
                recent_events: vec![super::super::protocol::RuntimeEvent::Text {
                    seq: 1,
                    chunk: "before".to_string(),
                }],
                recent_events_truncated: false,
                resume_state: crate::code_mode::cell::CellResumeState::default(),
                pending_resume_progress: CellResumeProgressDelta::default(),
            },
            prior_snapshot: CellDrainSnapshot {
                status: crate::code_mode::cell::CellStatus::Running,
                nested_tool_calls: 0,
                render_state: crate::code_mode::response::DrainRenderState {
                    output_text: "before".to_string(),
                    ..crate::code_mode::response::DrainRenderState::default()
                },
                truncated: false,
            },
            batch: DriverDrainBatch::progress(
                DrainRequest::poll_now(),
                vec![super::super::protocol::RuntimeEvent::ToolCallRequested(
                    super::super::protocol::ToolCallRequest {
                        seq: 2,
                        request_id: 7,
                        tool_name: "read_file".to_string(),
                        args_json: "{}".to_string(),
                    },
                )],
            ),
        };

        let resolution = pending.into_resolution().expect("resolution succeeds");
        assert!(matches!(
            resolution,
            PendingDrainResolution::Progress {
                active_cell,
                prior_snapshot,
                batch,
            }
                if active_cell.cell_id == "cell_progress_1"
                    && prior_snapshot.render_state.output_text == "before"
                    && batch.request == DrainRequest::poll_now()
                    && matches!(
                        batch.events.last(),
                        Some(super::super::protocol::RuntimeEvent::ToolCallRequested(
                            super::super::protocol::ToolCallRequest { request_id: 7, .. }
                        ))
                    )
        ));
    }

    #[tokio::test]
    async fn test_service_apply_terminal_wait_completion_from_pending_resolution() {
        let service = CodeModeService::default();
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        let first = service
            .execute(
                "session-h",
                r#"
text("before");
yield_control("pause");
text("after");
"#,
                Vec::new(),
                &mut invoke_tool,
            )
            .await
            .expect("exec yields");

        let pending = service
            .run_pending_cell_batch(
                "session-h",
                Some(&first.cell_id),
                DrainRequest::wait_for_next_event(),
                &mut invoke_tool,
            )
            .await
            .expect("pending drain batch exists");

        let PendingDrainResolution::Completion {
            active_cell,
            completion,
        } = pending.into_resolution().expect("resolution succeeds")
        else {
            panic!("expected terminal completion resolution");
        };

        let resumed = service
            .apply_terminal_wait_completion("session-h", active_cell, *completion)
            .await
            .expect("terminal completion is applied");

        assert!(!resumed.yielded);
        assert_eq!(resumed.output_text.trim(), "after");
    }

    #[tokio::test]
    async fn test_service_apply_in_progress_wait_batch_updates_active_cell_and_preserves_values() {
        let service = CodeModeService::default();
        let mut stored_values = HashMap::new();
        stored_values.insert("answer".to_string(), serde_json::json!(42));

        let active_cell = ActiveCellHandle {
            cell_id: "cell_progress_apply_1".to_string(),
            code: "tool_call()".to_string(),
            visible_tools: vec!["read_file".to_string()],
            status: crate::code_mode::cell::CellStatus::Running,
            last_event_seq: 1,
            recent_events: vec![super::super::protocol::RuntimeEvent::Text {
                seq: 1,
                chunk: "before".to_string(),
            }],
            recent_events_truncated: false,
            resume_state: crate::code_mode::cell::CellResumeState::default(),
            pending_resume_progress: CellResumeProgressDelta::default(),
        };

        service
            .persist_session_state(
                "session-progress",
                stored_values,
                Some(active_cell.clone()),
                None,
            )
            .await;

        let result = service
            .apply_in_progress_wait_batch(
                "session-progress",
                active_cell,
                CellDrainSnapshot {
                    status: crate::code_mode::cell::CellStatus::Running,
                    nested_tool_calls: 0,
                    render_state: crate::code_mode::response::DrainRenderState {
                        output_text: "before".to_string(),
                        ..crate::code_mode::response::DrainRenderState::default()
                    },
                    truncated: false,
                },
                DriverDrainBatch::progress(
                    DrainRequest::poll_now(),
                    vec![super::super::protocol::RuntimeEvent::ToolCallRequested(
                        super::super::protocol::ToolCallRequest {
                            seq: 2,
                            request_id: 11,
                            tool_name: "read_file".to_string(),
                            args_json: "{}".to_string(),
                        },
                    )],
                )
                .with_resume_progress(CellResumeProgressDelta {
                    replayed_tool_calls_delta: vec![runtime::callbacks::RecordedToolCall {
                        tool_name: "read_file".to_string(),
                        args_json: "{}".to_string(),
                        result_json: "{\"ok\":true}".to_string(),
                    }],
                    suppressed_text_calls_delta: 2,
                    total_nested_tool_calls_delta: 1,
                    ..CellResumeProgressDelta::default()
                }),
            )
            .await
            .expect("progress batch is applied");

        assert_eq!(result.cell_id, "cell_progress_apply_1");
        assert!(result.yielded);
        assert_eq!(result.output_text, "before");
        assert_eq!(result.nested_tool_calls, 1);

        let sessions = service.sessions.lock().await;
        let session = sessions
            .get("session-progress")
            .expect("session remains after progress apply");
        assert_eq!(
            session.stored_values.get("answer"),
            Some(&serde_json::json!(42))
        );
        let active_cell = session
            .active_cell
            .as_ref()
            .expect("progress apply keeps an active cell");
        assert_eq!(
            active_cell.status,
            crate::code_mode::cell::CellStatus::WaitingOnTool { request_id: 11 }
        );
        assert_eq!(active_cell.last_event_seq, 2);
        assert_eq!(active_cell.recent_events.len(), 2);
        assert!(active_cell.resume_state.replayed_tool_calls.is_empty());
        assert_eq!(active_cell.resume_state.suppressed_text_calls, 0);
        assert_eq!(active_cell.resume_state.total_nested_tool_calls, 0);
        assert_eq!(
            active_cell
                .pending_resume_progress
                .replayed_tool_calls_delta
                .len(),
            1
        );
        assert_eq!(
            active_cell
                .pending_resume_progress
                .suppressed_text_calls_delta,
            2
        );
        assert_eq!(
            active_cell
                .pending_resume_progress
                .total_nested_tool_calls_delta,
            1
        );
    }

    #[tokio::test]
    async fn test_service_apply_in_progress_wait_batch_uses_inferred_resume_progress_counts() {
        let service = CodeModeService::default();

        let active_cell = ActiveCellHandle {
            cell_id: "cell_progress_inferred_1".to_string(),
            code: "tool_call()".to_string(),
            visible_tools: vec!["read_file".to_string()],
            status: crate::code_mode::cell::CellStatus::Running,
            last_event_seq: 1,
            recent_events: Vec::new(),
            recent_events_truncated: false,
            resume_state: crate::code_mode::cell::CellResumeState::default(),
            pending_resume_progress: CellResumeProgressDelta::default(),
        };

        service
            .persist_session_state(
                "session-progress-inferred",
                HashMap::new(),
                Some(active_cell.clone()),
                None,
            )
            .await;

        let result = service
            .apply_in_progress_wait_batch(
                "session-progress-inferred",
                active_cell,
                CellDrainSnapshot {
                    status: crate::code_mode::cell::CellStatus::Running,
                    nested_tool_calls: 0,
                    render_state: crate::code_mode::response::DrainRenderState::default(),
                    truncated: false,
                },
                DriverDrainBatch::progress(
                    DrainRequest::poll_now(),
                    vec![
                        super::super::protocol::RuntimeEvent::Text {
                            seq: 2,
                            chunk: "after".to_string(),
                        },
                        super::super::protocol::RuntimeEvent::Notification {
                            seq: 3,
                            message: "done".to_string(),
                        },
                        super::super::protocol::RuntimeEvent::ToolCallRequested(
                            super::super::protocol::ToolCallRequest {
                                seq: 4,
                                request_id: 12,
                                tool_name: "read_file".to_string(),
                                args_json: "{}".to_string(),
                            },
                        ),
                        super::super::protocol::RuntimeEvent::Yield {
                            seq: 5,
                            kind: ExecYieldKind::Manual,
                            value: Some(serde_json::json!("pause")),
                            resume_after_ms: None,
                        },
                    ],
                ),
            )
            .await
            .expect("progress batch with inferred counters is applied");

        assert!(result.yielded);
        assert_eq!(result.output_text, "after");
        assert_eq!(result.notifications, vec!["done".to_string()]);
        assert_eq!(result.nested_tool_calls, 1);

        let sessions = service.sessions.lock().await;
        let session = sessions
            .get("session-progress-inferred")
            .expect("session remains after inferred progress apply");
        let active_cell = session
            .active_cell
            .as_ref()
            .expect("inferred progress apply keeps an active cell");
        assert_eq!(active_cell.resume_state.suppressed_text_calls, 0);
        assert_eq!(active_cell.resume_state.suppressed_notification_calls, 0);
        assert_eq!(active_cell.resume_state.skipped_yields, 0);
        assert_eq!(active_cell.resume_state.total_nested_tool_calls, 0);
        assert_eq!(
            active_cell
                .pending_resume_progress
                .suppressed_text_calls_delta,
            1
        );
        assert_eq!(
            active_cell
                .pending_resume_progress
                .suppressed_notification_calls_delta,
            1
        );
        assert_eq!(active_cell.pending_resume_progress.skipped_yields_delta, 1);
        assert_eq!(
            active_cell
                .pending_resume_progress
                .total_nested_tool_calls_delta,
            1
        );
    }

    #[tokio::test]
    async fn test_service_apply_pending_wait_resolution_progress_branch() {
        let service = CodeModeService::default();
        let mut stored_values = HashMap::new();
        stored_values.insert("answer".to_string(), serde_json::json!(42));

        service
            .persist_session_state(
                "session-progress-resolution",
                stored_values,
                Some(ActiveCellHandle {
                    cell_id: "cell_progress_resolution_1".to_string(),
                    code: "tool_call()".to_string(),
                    visible_tools: vec!["read_file".to_string()],
                    status: crate::code_mode::cell::CellStatus::Running,
                    last_event_seq: 1,
                    recent_events: vec![super::super::protocol::RuntimeEvent::Text {
                        seq: 1,
                        chunk: "before".to_string(),
                    }],
                    recent_events_truncated: false,
                    resume_state: crate::code_mode::cell::CellResumeState::default(),
                    pending_resume_progress: CellResumeProgressDelta::default(),
                }),
                None,
            )
            .await;

        let result = service
            .apply_pending_wait_resolution(
                "session-progress-resolution",
                PendingDrainResolution::Progress {
                    active_cell: ActiveCellHandle {
                        cell_id: "cell_progress_resolution_1".to_string(),
                        code: "tool_call()".to_string(),
                        visible_tools: vec!["read_file".to_string()],
                        status: crate::code_mode::cell::CellStatus::Running,
                        last_event_seq: 1,
                        recent_events: vec![super::super::protocol::RuntimeEvent::Text {
                            seq: 1,
                            chunk: "before".to_string(),
                        }],
                        recent_events_truncated: false,
                        resume_state: crate::code_mode::cell::CellResumeState::default(),
                        pending_resume_progress: CellResumeProgressDelta::default(),
                    },
                    prior_snapshot: CellDrainSnapshot {
                        status: crate::code_mode::cell::CellStatus::Running,
                        nested_tool_calls: 0,
                        render_state: crate::code_mode::response::DrainRenderState {
                            output_text: "before".to_string(),
                            ..crate::code_mode::response::DrainRenderState::default()
                        },
                        truncated: false,
                    },
                    batch: Box::new(DriverDrainBatch::progress(
                        DrainRequest::poll_now(),
                        vec![super::super::protocol::RuntimeEvent::ToolCallRequested(
                            super::super::protocol::ToolCallRequest {
                                seq: 2,
                                request_id: 21,
                                tool_name: "read_file".to_string(),
                                args_json: "{}".to_string(),
                            },
                        )],
                    )),
                },
            )
            .await
            .expect("progress resolution is applied");

        assert_eq!(result.cell_id, "cell_progress_resolution_1");
        assert!(result.yielded);

        let sessions = service.sessions.lock().await;
        let session = sessions
            .get("session-progress-resolution")
            .expect("session remains after resolution apply");
        assert_eq!(
            session.stored_values.get("answer"),
            Some(&serde_json::json!(42))
        );
        let active_cell = session
            .active_cell
            .as_ref()
            .expect("progress resolution keeps an active cell");
        assert_eq!(
            active_cell.status,
            crate::code_mode::cell::CellStatus::WaitingOnTool { request_id: 21 }
        );
    }

    #[tokio::test]
    async fn test_service_apply_execute_completion_from_driver_completion() {
        let service = CodeModeService::default();
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        let cell_id = "cell_exec_apply_1".to_string();
        let code = r#"
text("before");
yield_control("pause");
text("after");
"#
        .to_string();
        let completion = service
            .run_runtime_cell_batch_with_request(
                RuntimeBatchInvocation::for_execute(
                    cell_id.clone(),
                    code.clone(),
                    Vec::new(),
                    HashMap::new(),
                ),
                &mut invoke_tool,
            )
            .await
            .expect("batch is produced")
            .into_completion()
            .expect("completion is available");

        let result = service
            .apply_execute_completion("session-i", cell_id.clone(), code, Vec::new(), completion)
            .await
            .expect("execute completion is applied");

        assert_eq!(result.cell_id, cell_id);
        assert!(result.yielded);
        let sessions = service.sessions.lock().await;
        let active_cell = sessions
            .get("session-i")
            .and_then(|session| session.active_cell.as_ref())
            .expect("yielded execute retains an active cell");
        assert_eq!(active_cell.cell_id, result.cell_id);
        assert!(sessions
            .get("session-i")
            .and_then(|session| session.live_driver.as_ref())
            .is_some());
    }

    #[tokio::test]
    async fn test_service_session_state_helpers_persist_values_and_clear_active_cell() {
        let service = CodeModeService::default();
        let mut stored_values = HashMap::new();
        stored_values.insert("answer".to_string(), serde_json::json!(42));

        service
            .persist_session_state(
                "session-j",
                stored_values.clone(),
                Some(ActiveCellHandle {
                    cell_id: "cell_helper_1".to_string(),
                    code: "yield_control(\"pause\")".to_string(),
                    visible_tools: Vec::new(),
                    status: crate::code_mode::cell::CellStatus::Running,
                    last_event_seq: 1,
                    recent_events: Vec::new(),
                    recent_events_truncated: false,
                    resume_state: crate::code_mode::cell::CellResumeState::default(),
                    pending_resume_progress: CellResumeProgressDelta::default(),
                }),
                None,
            )
            .await;

        let sessions = service.sessions.lock().await;
        let session = sessions
            .get("session-j")
            .expect("session state is created by persist helper");
        assert_eq!(
            session.stored_values.get("answer"),
            Some(&serde_json::json!(42))
        );
        assert!(session.active_cell.is_some());
        drop(sessions);

        service.clear_active_cell("session-j").await;

        let sessions = service.sessions.lock().await;
        let session = sessions
            .get("session-j")
            .expect("session remains after active-cell clear");
        assert_eq!(
            session.stored_values.get("answer"),
            Some(&serde_json::json!(42))
        );
        assert!(session.active_cell.is_none());
    }

    #[test]
    fn test_runtime_batch_invocation_builders_preserve_execute_and_pending_cell_setup() {
        let mut stored_values = HashMap::new();
        stored_values.insert("answer".to_string(), serde_json::json!(42));

        let execute_invocation = RuntimeBatchInvocation::for_execute(
            "cell_exec_1".to_string(),
            "text(\"hello\")".to_string(),
            vec!["read_file".to_string()],
            stored_values.clone(),
        );

        assert_eq!(execute_invocation.cell_id, "cell_exec_1");
        assert_eq!(execute_invocation.code, "text(\"hello\")");
        assert_eq!(
            execute_invocation.visible_tools,
            vec!["read_file".to_string()]
        );
        assert_eq!(execute_invocation.request, DrainRequest::to_completion());
        assert!(execute_invocation
            .resume_state
            .replayed_tool_calls
            .is_empty());
        assert!(execute_invocation
            .resume_state
            .recorded_timer_calls
            .is_empty());
        assert_eq!(execute_invocation.resume_state.skipped_yields, 0);
        assert_eq!(execute_invocation.resume_state.suppressed_text_calls, 0);
        assert_eq!(
            execute_invocation
                .resume_state
                .suppressed_notification_calls,
            0
        );
        assert_eq!(
            execute_invocation.stored_values.get("answer"),
            Some(&serde_json::json!(42))
        );

        let custom_execute_invocation = RuntimeBatchInvocation::for_execute_with_request(
            "cell_exec_2".to_string(),
            "text(\"wait\")".to_string(),
            Vec::new(),
            HashMap::new(),
            DrainRequest::wait_for_next_event(),
        );
        assert_eq!(
            custom_execute_invocation.request,
            DrainRequest::wait_for_next_event()
        );

        let active_cell = ActiveCellHandle {
            cell_id: "cell_wait_1".to_string(),
            code: "yield_control(\"pause\")".to_string(),
            visible_tools: vec!["write_file".to_string()],
            status: crate::code_mode::cell::CellStatus::Running,
            last_event_seq: 3,
            recent_events: Vec::new(),
            recent_events_truncated: false,
            resume_state: crate::code_mode::cell::CellResumeState {
                skipped_yields: 2,
                suppressed_text_calls: 1,
                ..crate::code_mode::cell::CellResumeState::default()
            },
            pending_resume_progress: CellResumeProgressDelta::default(),
        };

        let pending_invocation = RuntimeBatchInvocation::for_pending_cell(
            &active_cell,
            stored_values,
            DrainRequest::wait_for_next_event(),
        );

        assert_eq!(pending_invocation.cell_id, active_cell.cell_id);
        assert_eq!(pending_invocation.code, active_cell.code);
        assert_eq!(pending_invocation.visible_tools, active_cell.visible_tools);
        assert_eq!(
            pending_invocation.request,
            DrainRequest::wait_for_next_event()
        );
        let expected_resume_state = active_cell.runtime_resume_state();
        assert_eq!(
            pending_invocation.resume_state.replayed_tool_calls,
            expected_resume_state.replayed_tool_calls
        );
        assert_eq!(
            pending_invocation.resume_state.recorded_timer_calls,
            expected_resume_state.recorded_timer_calls
        );
        assert_eq!(
            pending_invocation.resume_state.skipped_yields,
            expected_resume_state.skipped_yields
        );
        assert_eq!(
            pending_invocation.resume_state.suppressed_text_calls,
            expected_resume_state.suppressed_text_calls
        );
        assert_eq!(
            pending_invocation
                .resume_state
                .suppressed_notification_calls,
            expected_resume_state.suppressed_notification_calls
        );
        assert_eq!(
            pending_invocation.stored_values.get("answer"),
            Some(&serde_json::json!(42))
        );
    }

    #[test]
    fn test_pending_drain_batch_falls_back_to_prior_snapshot_for_empty_wait_batch() {
        let pending = PendingDrainBatch {
            active_cell: ActiveCellHandle {
                cell_id: "cell_fallback_1".to_string(),
                code: "yield_control(\"pause\")".to_string(),
                visible_tools: Vec::new(),
                status: crate::code_mode::cell::CellStatus::Running,
                last_event_seq: 2,
                recent_events: vec![super::super::protocol::RuntimeEvent::Yield {
                    seq: 2,
                    kind: ExecYieldKind::Manual,
                    value: Some(serde_json::json!("pause")),
                    resume_after_ms: None,
                }],
                recent_events_truncated: false,
                resume_state: crate::code_mode::cell::CellResumeState::default(),
                pending_resume_progress: CellResumeProgressDelta::default(),
            },
            prior_snapshot: CellDrainSnapshot {
                status: crate::code_mode::cell::CellStatus::Running,
                nested_tool_calls: 0,
                render_state: crate::code_mode::response::DrainRenderState {
                    yield_kind: Some(ExecYieldKind::Manual),
                    yield_value: Some(serde_json::json!("pause")),
                    ..crate::code_mode::response::DrainRenderState::default()
                },
                truncated: false,
            },
            batch: DriverDrainBatch::empty(DrainRequest::poll_now()),
        };

        assert!(pending.should_fallback_to_prior_snapshot());
        assert!(pending
            .prior_snapshot
            .render(&pending.active_cell.cell_id)
            .contains("yielded after 0 nested tool call(s)"));
        let resolution = pending.into_resolution().expect("resolution succeeds");
        assert!(matches!(
            resolution,
            PendingDrainResolution::Progress {
                active_cell,
                prior_snapshot,
                batch,
            }
                if active_cell.cell_id == "cell_fallback_1"
                    && prior_snapshot.render_state.yield_kind == Some(ExecYieldKind::Manual)
                    && batch.is_empty()
        ));
    }

    #[tokio::test]
    async fn test_service_run_runtime_cell_batch_exposes_terminal_result() {
        let service = CodeModeService::default();
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        let batch = service
            .run_runtime_cell_batch_with_request(
                RuntimeBatchInvocation::for_execute_with_request(
                    "cell_batch_1".to_string(),
                    "text(\"hello\")".to_string(),
                    Vec::new(),
                    HashMap::new(),
                    DrainRequest::wait_for_next_event(),
                ),
                &mut invoke_tool,
            )
            .await
            .expect("service returns a driver drain batch");

        assert_eq!(batch.request, DrainRequest::wait_for_next_event());
        assert!(batch.terminal_result.is_some());
        assert!(matches!(
            batch.events.last(),
            Some(super::super::protocol::RuntimeEvent::Completed { .. })
        ));
    }

    #[tokio::test]
    async fn test_service_poll_reuses_live_worker_without_replaying_tool_side_effects() {
        let service = CodeModeService::default();
        let calls = StdArc::new(StdMutex::new(Vec::<String>::new()));
        let calls_for_tool = calls.clone();
        let mut invoke_tool = move |tool: String, args_json: String| {
            let calls_for_tool = calls_for_tool.clone();
            async move {
                calls_for_tool
                    .lock()
                    .unwrap()
                    .push(format!("{}:{}", tool, args_json));
                Ok(serde_json::json!({
                    "value": serde_json::from_str::<serde_json::Value>(&args_json)
                        .unwrap()
                        .get("value")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default(),
                })
                .to_string())
            }
        };

        let first = service
            .execute(
                "session-poll-reuse",
                r#"
yield_control("resume");
const first = await tools.echo_tool({ value: "hello" });
const target = Date.now() + 40;
while (Date.now() < target) {}
text(first.value);
yield_control("done");
"#,
                vec!["echo_tool".to_string()],
                &mut invoke_tool,
            )
            .await
            .expect("initial exec yields");

        let initial_poll = service
            .poll("session-poll-reuse", Some(&first.cell_id), &mut invoke_tool)
            .await
            .expect("first poll starts the live worker");

        assert!(initial_poll.yielded);

        tokio::time::sleep(Duration::from_millis(5)).await;

        let polled = service
            .poll("session-poll-reuse", Some(&first.cell_id), &mut invoke_tool)
            .await
            .expect("second poll returns non-terminal progress");

        assert!(polled.yielded);
        assert_eq!(
            initial_poll.nested_tool_calls.max(polled.nested_tool_calls),
            1
        );
        assert_eq!(calls.lock().unwrap().len(), 1);

        let resumed = service
            .wait("session-poll-reuse", Some(&first.cell_id), &mut invoke_tool)
            .await
            .expect("wait resumes the existing live worker");

        assert!(resumed.yielded);
        assert_eq!(resumed.yield_value, Some(serde_json::json!("done")));
        assert_eq!(resumed.output_text.trim(), "hello");
        assert_eq!(resumed.nested_tool_calls, 1);
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_service_poll_persists_timer_progress_without_visible_events() {
        let service = CodeModeService::default();
        let mut invoke_tool =
            |_tool: String, _args: String| async move { Ok("\"unused\"".to_string()) };

        let first = service
            .execute(
                "session-poll-timer",
                r#"
yield_control("resume");
setTimeout(async () => {
  text("later");
}, 20);
const target = Date.now() + 40;
while (Date.now() < target) {}
"#,
                Vec::new(),
                &mut invoke_tool,
            )
            .await
            .expect("initial exec yields");

        let initial_poll = service
            .poll("session-poll-timer", Some(&first.cell_id), &mut invoke_tool)
            .await
            .expect("first poll starts the live worker");

        assert!(initial_poll.yielded);

        tokio::time::sleep(Duration::from_millis(5)).await;

        let polled = service
            .poll("session-poll-timer", Some(&first.cell_id), &mut invoke_tool)
            .await
            .expect("second poll captures hidden timer progress");

        assert!(polled.yielded);

        let sessions = service.sessions.lock().await;
        let session = sessions
            .get("session-poll-timer")
            .expect("session is retained after poll");
        let active_cell = session
            .active_cell
            .as_ref()
            .expect("active cell remains after empty poll");
        assert_eq!(
            active_cell
                .pending_resume_progress
                .recorded_timer_calls
                .as_ref()
                .map(Vec::len),
            Some(1)
        );
        assert!(session.live_driver.is_some());
        drop(sessions);

        let resumed = service
            .wait("session-poll-timer", Some(&first.cell_id), &mut invoke_tool)
            .await
            .expect("wait drains the same live worker");

        assert!(resumed.yielded);
        assert_eq!(resumed.yield_kind, Some(ExecYieldKind::Timer));

        let sessions = service.sessions.lock().await;
        let active_cell = sessions
            .get("session-poll-timer")
            .and_then(|session| session.active_cell.as_ref())
            .expect("timer yield retains the active cell");
        assert_eq!(
            active_cell
                .pending_resume_progress
                .recorded_timer_calls
                .as_ref()
                .map(Vec::len),
            Some(1)
        );
        assert!(sessions
            .get("session-poll-timer")
            .and_then(|session| session.live_driver.as_ref())
            .is_some());
    }
}
