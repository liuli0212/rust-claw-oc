use std::sync::Mutex;

use once_cell::sync::Lazy;
use rusqlite::{params, Connection};

use super::model::{RunSummary, TraceActor, TraceKind, TraceLevel, TraceRecord, TraceStatus};
use crate::schema::StoragePaths;

static TRACE_DB: Lazy<Mutex<Option<Connection>>> = Lazy::new(|| Mutex::new(None));

pub(crate) fn persist_record(record: &TraceRecord) {
    let _ = with_conn(|conn| {
        conn.execute(
            r#"
            INSERT OR REPLACE INTO trace_records (
                record_id, trace_id, run_id, span_id, parent_span_id, session_id, task_id, turn_id,
                actor, kind, name, status, iteration, ts_unix_ms, duration_ms, level, summary, attrs_json
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18
            )
            "#,
            params![
                record.record_id,
                record.trace_id,
                record.run_id,
                record.span_id,
                record.parent_span_id,
                record.session_id,
                record.task_id,
                record.turn_id,
                record.actor.as_str(),
                trace_kind_as_str(&record.kind),
                record.name,
                record.status.as_str(),
                record.iteration,
                record.ts_unix_ms as i64,
                record.duration_ms.map(|v| v as i64),
                trace_level_as_str(&record.level),
                record.summary,
                serde_json::to_string(&record.attrs).unwrap_or_else(|_| "{}".to_string()),
            ],
        )?;
        Ok(())
    });
}

pub(crate) fn persist_run(summary: &RunSummary) {
    let _ = with_conn(|conn| {
        conn.execute(
            r#"
            INSERT OR REPLACE INTO runs (
                run_id, trace_id, session_id, root_session_id, task_id, root_goal, status,
                started_at_unix_ms, finished_at_unix_ms, duration_ms, provider, model,
                total_events, total_spans, total_tool_calls, total_llm_calls, total_subagents,
                peak_prompt_tokens, peak_history_tokens, last_error_summary, tool_names_json,
                artifact_paths_json, updated_at_unix_ms
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                ?8, ?9, ?10, ?11, ?12,
                ?13, ?14, ?15, ?16, ?17,
                ?18, ?19, ?20, ?21,
                ?22, ?23
            )
            "#,
            params![
                summary.run_id,
                summary.trace_id,
                summary.session_id,
                summary.root_session_id,
                summary.task_id,
                summary.root_goal,
                summary.status,
                summary.started_at_unix_ms as i64,
                summary.finished_at_unix_ms.map(|v| v as i64),
                summary.duration_ms.map(|v| v as i64),
                summary.provider,
                summary.model,
                summary.total_events as i64,
                summary.total_spans as i64,
                summary.total_tool_calls as i64,
                summary.total_llm_calls as i64,
                summary.total_subagents as i64,
                summary.peak_prompt_tokens.map(|v| v as i64),
                summary.peak_history_tokens.map(|v| v as i64),
                summary.last_error_summary,
                serde_json::to_string(&summary.tool_names).unwrap_or_else(|_| "[]".to_string()),
                serde_json::to_string(&summary.artifact_paths).unwrap_or_else(|_| "[]".to_string()),
                summary.updated_at_unix_ms as i64,
            ],
        )?;
        Ok(())
    });
}

pub(crate) fn list_runs() -> Option<Vec<RunSummary>> {
    with_conn(|conn| {
        let mut stmt = conn.prepare(
            r#"
            SELECT run_id, trace_id, session_id, root_session_id, task_id, root_goal, status,
                   started_at_unix_ms, finished_at_unix_ms, duration_ms, provider, model,
                   total_events, total_spans, total_tool_calls, total_llm_calls, total_subagents,
                   peak_prompt_tokens, peak_history_tokens, last_error_summary, tool_names_json,
                   artifact_paths_json, updated_at_unix_ms
            FROM runs
            ORDER BY started_at_unix_ms DESC
            "#,
        )?;
        let rows = stmt.query_map([], row_to_run_summary)?;
        let mut runs = Vec::new();
        for row in rows.flatten() {
            runs.push(row);
        }
        Ok(runs)
    })
}

pub(crate) fn get_run(run_id: &str) -> Option<RunSummary> {
    with_conn(|conn| {
        let mut stmt = conn.prepare(
            r#"
            SELECT run_id, trace_id, session_id, root_session_id, task_id, root_goal, status,
                   started_at_unix_ms, finished_at_unix_ms, duration_ms, provider, model,
                   total_events, total_spans, total_tool_calls, total_llm_calls, total_subagents,
                   peak_prompt_tokens, peak_history_tokens, last_error_summary, tool_names_json,
                   artifact_paths_json, updated_at_unix_ms
            FROM runs
            WHERE run_id = ?1
            "#,
        )?;
        Ok(stmt.query_row([run_id], row_to_run_summary).ok())
    })
    .flatten()
}

pub(crate) fn get_records(run_id: &str) -> Option<Vec<TraceRecord>> {
    with_conn(|conn| {
        let mut stmt = conn.prepare(
            r#"
            SELECT record_id, trace_id, run_id, span_id, parent_span_id, session_id, task_id, turn_id,
                   actor, kind, name, status, iteration, ts_unix_ms, duration_ms, level, summary, attrs_json
            FROM trace_records
            WHERE run_id = ?1
            ORDER BY ts_unix_ms ASC, rowid ASC
            "#,
        )?;
        let rows = stmt.query_map([run_id], row_to_trace_record)?;
        let mut records = Vec::new();
        for row in rows.flatten() {
            records.push(row);
        }
        Ok(records)
    })
}

fn with_conn<T, F>(f: F) -> Option<T>
where
    F: FnOnce(&Connection) -> rusqlite::Result<T>,
{
    let mut guard = TRACE_DB.lock().ok()?;
    if guard.is_none() {
        *guard = open_connection().ok();
    }
    let conn = guard.as_ref()?;
    f(conn).ok()
}

fn open_connection() -> rusqlite::Result<Connection> {
    let path = StoragePaths::trace_index_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;
    init_schema(&conn)?;
    Ok(conn)
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS runs (
            run_id TEXT PRIMARY KEY,
            trace_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            root_session_id TEXT NOT NULL,
            task_id TEXT,
            root_goal TEXT,
            status TEXT NOT NULL,
            started_at_unix_ms INTEGER NOT NULL,
            finished_at_unix_ms INTEGER,
            duration_ms INTEGER,
            provider TEXT,
            model TEXT,
            total_events INTEGER DEFAULT 0,
            total_spans INTEGER DEFAULT 0,
            total_tool_calls INTEGER DEFAULT 0,
            total_llm_calls INTEGER DEFAULT 0,
            total_subagents INTEGER DEFAULT 0,
            peak_prompt_tokens INTEGER,
            peak_history_tokens INTEGER,
            last_error_summary TEXT,
            tool_names_json TEXT NOT NULL DEFAULT '[]',
            artifact_paths_json TEXT NOT NULL DEFAULT '[]',
            updated_at_unix_ms INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS trace_records (
            record_id TEXT PRIMARY KEY,
            trace_id TEXT NOT NULL,
            run_id TEXT NOT NULL,
            span_id TEXT,
            parent_span_id TEXT,
            session_id TEXT NOT NULL,
            task_id TEXT,
            turn_id TEXT,
            actor TEXT NOT NULL,
            kind TEXT NOT NULL,
            name TEXT NOT NULL,
            status TEXT NOT NULL,
            iteration INTEGER,
            ts_unix_ms INTEGER NOT NULL,
            duration_ms INTEGER,
            level TEXT NOT NULL,
            summary TEXT,
            attrs_json TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_trace_records_run_ts
            ON trace_records(run_id, ts_unix_ms);
        CREATE INDEX IF NOT EXISTS idx_trace_records_session_ts
            ON trace_records(session_id, ts_unix_ms);
        CREATE INDEX IF NOT EXISTS idx_trace_records_name_ts
            ON trace_records(name, ts_unix_ms);
        CREATE INDEX IF NOT EXISTS idx_trace_records_status
            ON trace_records(status);
        CREATE INDEX IF NOT EXISTS idx_trace_records_turn
            ON trace_records(turn_id);
        CREATE INDEX IF NOT EXISTS idx_runs_session_started
            ON runs(session_id, started_at_unix_ms);
        CREATE INDEX IF NOT EXISTS idx_runs_root_session_started
            ON runs(root_session_id, started_at_unix_ms);
        "#,
    )?;
    Ok(())
}

fn row_to_run_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunSummary> {
    let tool_names_json: String = row.get(20)?;
    let artifact_paths_json: String = row.get(21)?;
    Ok(RunSummary {
        run_id: row.get(0)?,
        trace_id: row.get(1)?,
        session_id: row.get(2)?,
        root_session_id: row.get(3)?,
        task_id: row.get(4)?,
        root_goal: row.get(5)?,
        status: row.get(6)?,
        started_at_unix_ms: row.get::<_, i64>(7)? as u64,
        finished_at_unix_ms: row.get::<_, Option<i64>>(8)?.map(|v| v as u64),
        duration_ms: row.get::<_, Option<i64>>(9)?.map(|v| v as u64),
        provider: row.get(10)?,
        model: row.get(11)?,
        total_events: row.get::<_, i64>(12)? as u64,
        total_spans: row.get::<_, i64>(13)? as u64,
        total_tool_calls: row.get::<_, i64>(14)? as u64,
        total_llm_calls: row.get::<_, i64>(15)? as u64,
        total_subagents: row.get::<_, i64>(16)? as u64,
        peak_prompt_tokens: row.get::<_, Option<i64>>(17)?.map(|v| v as u64),
        peak_history_tokens: row.get::<_, Option<i64>>(18)?.map(|v| v as u64),
        last_error_summary: row.get(19)?,
        tool_names: serde_json::from_str(&tool_names_json).unwrap_or_default(),
        artifact_paths: serde_json::from_str(&artifact_paths_json).unwrap_or_default(),
        updated_at_unix_ms: row.get::<_, i64>(22)? as u64,
    })
}

fn row_to_trace_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<TraceRecord> {
    let attrs_json: String = row.get(17)?;
    Ok(TraceRecord {
        schema_version: crate::schema::CURRENT_SCHEMA_VERSION,
        record_id: row.get(0)?,
        trace_id: row.get(1)?,
        run_id: row.get(2)?,
        span_id: row.get(3)?,
        parent_span_id: row.get(4)?,
        session_id: row.get(5)?,
        task_id: row.get(6)?,
        turn_id: row.get(7)?,
        actor: actor_from_str(&row.get::<_, String>(8)?),
        kind: kind_from_str(&row.get::<_, String>(9)?),
        name: row.get(10)?,
        status: status_from_str(&row.get::<_, String>(11)?),
        iteration: row.get::<_, Option<i64>>(12)?.map(|v| v as u32),
        ts_unix_ms: row.get::<_, i64>(13)? as u64,
        duration_ms: row.get::<_, Option<i64>>(14)?.map(|v| v as u64),
        level: level_from_str(&row.get::<_, String>(15)?),
        summary: row.get(16)?,
        attrs: serde_json::from_str(&attrs_json).unwrap_or_else(|_| serde_json::json!({})),
    })
}

fn trace_kind_as_str(kind: &TraceKind) -> &'static str {
    match kind {
        TraceKind::SpanStart => "span_start",
        TraceKind::SpanEnd => "span_end",
        TraceKind::Event => "event",
    }
}

fn trace_level_as_str(level: &TraceLevel) -> &'static str {
    match level {
        TraceLevel::Normal => "normal",
        TraceLevel::Debug => "debug",
    }
}

fn actor_from_str(value: &str) -> TraceActor {
    match value {
        "user" => TraceActor::User,
        "main_agent" => TraceActor::MainAgent,
        "subagent" => TraceActor::Subagent,
        "skill" => TraceActor::Skill,
        "llm" => TraceActor::Llm,
        "tool" => TraceActor::Tool,
        "context" => TraceActor::Context,
        "scheduler" => TraceActor::Scheduler,
        _ => TraceActor::System,
    }
}

fn kind_from_str(value: &str) -> TraceKind {
    match value {
        "span_start" => TraceKind::SpanStart,
        "span_end" => TraceKind::SpanEnd,
        _ => TraceKind::Event,
    }
}

fn status_from_str(value: &str) -> TraceStatus {
    match value {
        "ok" => TraceStatus::Ok,
        "error" => TraceStatus::Error,
        "cancelled" => TraceStatus::Cancelled,
        "timed_out" => TraceStatus::TimedOut,
        "yielded" => TraceStatus::Yielded,
        "retrying" => TraceStatus::Retrying,
        "skipped" => TraceStatus::Skipped,
        _ => TraceStatus::Running,
    }
}

fn level_from_str(value: &str) -> TraceLevel {
    match value {
        "debug" => TraceLevel::Debug,
        _ => TraceLevel::Normal,
    }
}
