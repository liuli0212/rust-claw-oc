use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::sync::broadcast;
use uuid::Uuid;

use super::model::{
    RunSummary, TraceActor, TraceContext, TraceKind, TraceLevel, TraceRecord, TraceSeed,
    TraceStatus,
};
use crate::schema::{StoragePaths, CURRENT_SCHEMA_VERSION};

pub fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub struct RecordPayload {
    pub status: TraceStatus,
    pub summary: Option<String>,
    pub attrs: Value,
}

pub struct RecordTiming {
    pub span_id: Option<String>,
    pub parent_span_id: Option<String>,
    pub ts_unix_ms: u64,
    pub duration_ms: Option<u64>,
}

#[derive(Clone)]
pub struct TraceBus {
    inner: Arc<TraceBusInner>,
}

struct TraceBusInner {
    live_tx: broadcast::Sender<TraceRecord>,
    file_writers: Mutex<HashMap<String, File>>,
    summary_cache: Mutex<HashMap<String, RunSummary>>,
}

impl Default for TraceBus {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceBus {
    pub fn new() -> Self {
        let (live_tx, _) = broadcast::channel(2048);
        Self {
            inner: Arc::new(TraceBusInner {
                live_tx,
                file_writers: Mutex::new(HashMap::new()),
                summary_cache: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TraceRecord> {
        self.inner.live_tx.subscribe()
    }

    pub fn start_span(
        &self,
        ctx: &TraceContext,
        actor: TraceActor,
        name: &str,
        attrs: Value,
    ) -> TraceSpanHandle {
        let span_id = format!("spn_{}", Uuid::new_v4().simple());
        let started_at_unix_ms = unix_ms_now();

        let record = self.build_record(
            ctx,
            actor.clone(),
            TraceKind::SpanStart,
            name.to_string(),
            RecordPayload {
                status: TraceStatus::Running,
                summary: None,
                attrs,
            },
            RecordTiming {
                span_id: Some(span_id.clone()),
                parent_span_id: ctx.parent_span_id.clone(),
                ts_unix_ms: started_at_unix_ms,
                duration_ms: None,
            },
        );
        self.publish(record);

        TraceSpanHandle {
            bus: self.clone(),
            ctx: ctx.clone(),
            actor,
            span_id,
            started_at_unix_ms,
        }
    }

    pub fn record_event(
        &self,
        ctx: &TraceContext,
        actor: TraceActor,
        name: impl Into<String>,
        status: TraceStatus,
        summary: Option<String>,
        attrs: Value,
    ) {
        let record = self.build_record(
            ctx,
            actor,
            TraceKind::Event,
            name.into(),
            RecordPayload { status, summary, attrs },
            RecordTiming {
                span_id: None,
                parent_span_id: ctx.parent_span_id.clone(),
                ts_unix_ms: unix_ms_now(),
                duration_ms: None,
            },
        );
        self.publish(record);
    }

    #[allow(clippy::too_many_arguments)]
    fn build_record(
        &self,
        ctx: &TraceContext,
        actor: TraceActor,
        kind: TraceKind,
        name: String,
        payload: RecordPayload,
        timing: RecordTiming,
    ) -> TraceRecord {
        let attrs = inject_root_session(payload.attrs, &ctx.root_session_id);
        TraceRecord {
            schema_version: CURRENT_SCHEMA_VERSION,
            record_id: format!("trc_{}", Uuid::new_v4().simple()),
            trace_id: ctx.trace_id.clone(),
            run_id: ctx.run_id.clone(),
            span_id: timing.span_id,
            parent_span_id: timing.parent_span_id,
            session_id: ctx.session_id.clone(),
            task_id: ctx.task_id.clone(),
            turn_id: ctx.turn_id.clone(),
            iteration: ctx.iteration,
            actor,
            kind,
            name,
            status: payload.status,
            ts_unix_ms: timing.ts_unix_ms,
            duration_ms: timing.duration_ms,
            level: TraceLevel::Normal,
            summary: payload.summary,
            attrs,
        }
    }

    fn publish(&self, record: TraceRecord) {
        let _ = self.inner.live_tx.send(record.clone());
        self.persist_record(&record);
        self.update_summary(&record);
    }

    fn persist_record(&self, record: &TraceRecord) {
        let line = match serde_json::to_string(record) {
            Ok(line) => line,
            Err(error) => {
                tracing::warn!("failed to serialize trace record: {}", error);
                return;
            }
        };

        let path = StoragePaths::trace_run_records_file(&record.run_id);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let mut writers = self.inner.file_writers.lock().unwrap();
        let file = writers.entry(record.run_id.clone()).or_insert_with(|| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap_or_else(|error| panic!("failed to open trace file {:?}: {}", path, error))
        });

        if let Err(error) = file.write_all(line.as_bytes()) {
            tracing::warn!("failed to write trace record: {}", error);
            return;
        }
        if let Err(error) = file.write_all(b"\n") {
            tracing::warn!("failed to write trace newline: {}", error);
        }
        super::sqlite::persist_record(record);
    }

    fn update_summary(&self, record: &TraceRecord) {
        let root_session_id =
            root_session_id_from_attrs(&record.attrs).unwrap_or_else(|| record.session_id.clone());
        let mut cache = self.inner.summary_cache.lock().unwrap();
        let summary = cache.entry(record.run_id.clone()).or_insert_with(|| {
            load_summary(&record.run_id).unwrap_or_else(|| {
                RunSummary::new(
                    &record.run_id,
                    &record.trace_id,
                    &record.session_id,
                    &root_session_id,
                )
            })
        });

        summary.updated_at_unix_ms = record.ts_unix_ms;
        summary.trace_id = record.trace_id.clone();
        summary.task_id = record.task_id.clone().or_else(|| summary.task_id.clone());
        summary.root_session_id = root_session_id;
        if summary.session_id.is_empty() {
            summary.session_id = record.session_id.clone();
        }
        summary.total_events += 1;
        if record.kind == TraceKind::SpanStart {
            summary.total_spans += 1;
        }

        if matches!(record.actor, TraceActor::Llm) && record.name == "llm_request_started" {
            summary.total_llm_calls += 1;
        }

        if matches!(record.actor, TraceActor::Tool) && record.name == "tool_started" {
            summary.total_tool_calls += 1;
            if let Some(tool_name) = record.attrs.get("tool_name").and_then(Value::as_str) {
                push_unique(&mut summary.tool_names, tool_name.to_string());
            }
        }

        if matches!(record.actor, TraceActor::Subagent) && record.name == "subagent_spawned" {
            summary.total_subagents += 1;
        }

        if record.name == "run_started" {
            summary.session_id = record.session_id.clone();
            summary.started_at_unix_ms = record.ts_unix_ms;
            summary.status = "running".to_string();
            summary.root_goal = record
                .attrs
                .get("goal")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| summary.root_goal.clone());
            summary.provider = record
                .attrs
                .get("provider")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| summary.provider.clone());
            summary.model = record
                .attrs
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| summary.model.clone());
        }

        if record.name == "run_finished"
            || record.name == "run_failed"
            || record.name == "run_cancelled"
        {
            summary.finished_at_unix_ms = Some(record.ts_unix_ms);
            summary.duration_ms = record.duration_ms.or_else(|| {
                summary
                    .started_at_unix_ms
                    .checked_sub(0)
                    .map(|_| record.ts_unix_ms.saturating_sub(summary.started_at_unix_ms))
            });
            summary.status = match record.name.as_str() {
                "run_failed" => "failed".to_string(),
                "run_cancelled" => "cancelled".to_string(),
                _ => "finished".to_string(),
            };
        }

        if let Some(prompt_tokens) = record
            .attrs
            .get("approx_prompt_tokens")
            .and_then(Value::as_u64)
        {
            summary.peak_prompt_tokens = Some(
                summary
                    .peak_prompt_tokens
                    .unwrap_or_default()
                    .max(prompt_tokens),
            );
        }

        if let Some(history_tokens) = record.attrs.get("history_tokens").and_then(Value::as_u64) {
            summary.peak_history_tokens = Some(
                summary
                    .peak_history_tokens
                    .unwrap_or_default()
                    .max(history_tokens),
            );
        }

        if let Some(path) = record.attrs.get("file_path").and_then(Value::as_str) {
            push_unique(&mut summary.artifact_paths, path.to_string());
        }

        if matches!(
            record.status,
            TraceStatus::Error | TraceStatus::TimedOut | TraceStatus::Cancelled
        ) {
            summary.last_error_summary = record.summary.clone().or_else(|| {
                record
                    .attrs
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        }

        persist_summary(summary);
        super::sqlite::persist_run(summary);
    }
}

impl Default for TraceBus {
    fn default() -> Self {
        Self::new()
    }
}

pub struct TraceSpanHandle {
    bus: TraceBus,
    ctx: TraceContext,
    actor: TraceActor,
    span_id: String,
    started_at_unix_ms: u64,
}

impl TraceSpanHandle {
    pub fn span_id(&self) -> &str {
        &self.span_id
    }

    pub fn child_context(&self) -> TraceContext {
        self.ctx.with_parent_span_id(Some(self.span_id.clone()))
    }

    pub fn finish(
        self,
        end_name: impl Into<String>,
        status: TraceStatus,
        summary: Option<String>,
        attrs: Value,
    ) {
        let ended_at_unix_ms = unix_ms_now();
        let record = self.bus.build_record(
            &self.ctx,
            self.actor,
            TraceKind::SpanEnd,
            end_name.into(),
            RecordPayload { status, summary, attrs },
            RecordTiming {
                span_id: Some(self.span_id),
                parent_span_id: self.ctx.parent_span_id.clone(),
                ts_unix_ms: ended_at_unix_ms,
                duration_ms: Some(ended_at_unix_ms.saturating_sub(self.started_at_unix_ms)),
            },
        );
        self.bus.publish(record);
    }
}

fn push_unique(values: &mut Vec<String>, candidate: String) {
    if !values.iter().any(|value| value == &candidate) {
        values.push(candidate);
    }
}

fn inject_root_session(attrs: Value, root_session_id: &str) -> Value {
    let mut attrs = attrs.as_object().cloned().unwrap_or_default();
    attrs
        .entry("root_session_id".to_string())
        .or_insert_with(|| Value::String(root_session_id.to_string()));
    Value::Object(attrs)
}

fn root_session_id_from_attrs(attrs: &Value) -> Option<String> {
    attrs
        .get("root_session_id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn load_summary(run_id: &str) -> Option<RunSummary> {
    let path = StoragePaths::trace_run_summary_file(run_id);
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn persist_summary(summary: &RunSummary) {
    let path = StoragePaths::trace_run_summary_file(&summary.run_id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(summary) {
        Ok(content) => {
            if let Err(error) = std::fs::write(path, content) {
                tracing::warn!("failed to persist trace run summary: {}", error);
            }
        }
        Err(error) => tracing::warn!("failed to serialize trace run summary: {}", error),
    }
}

pub fn trace_ctx_from_seed(seed: &TraceSeed, session_id: &str) -> TraceContext {
    TraceContext {
        trace_id: seed.trace_id.clone(),
        run_id: seed.run_id.clone(),
        session_id: session_id.to_string(),
        root_session_id: seed.root_session_id.clone(),
        task_id: seed.task_id.clone(),
        turn_id: None,
        iteration: None,
        parent_span_id: seed.parent_span_id.clone(),
    }
}

pub fn make_attrs(pairs: Vec<(&str, Value)>) -> Value {
    let mut object = serde_json::Map::new();
    for (key, value) in pairs {
        object.insert(key.to_string(), value);
    }
    json!(object)
}
