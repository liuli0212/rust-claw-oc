use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::model::{
    RunContextOverview, RunOverview, RunSummary, TraceArtifacts, TraceHotspot, TraceRecord,
    TraceSpanSummary, TraceTreeNode,
};
use crate::schema::StoragePaths;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunQuery {
    pub session_id: Option<String>,
    pub task_id: Option<String>,
    pub status: Option<String>,
    pub tool_name: Option<String>,
    pub query: Option<String>,
    pub from_unix_ms: Option<u64>,
    pub to_unix_ms: Option<u64>,
    pub min_duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecordQuery {
    pub actor: Option<String>,
    pub name: Option<String>,
    pub status: Option<String>,
    pub turn_id: Option<String>,
    pub iteration: Option<u32>,
}

pub fn list_runs(query: &RunQuery) -> Vec<RunSummary> {
    let mut runs = sqlite_or_json_runs();
    runs.retain(|summary| run_matches(summary, query));
    runs.sort_by_key(|run| std::cmp::Reverse(run.started_at_unix_ms));
    runs
}

pub fn get_run(run_id: &str) -> Option<RunSummary> {
    if let Some(run) = super::sqlite::get_run(run_id) {
        return Some(run);
    }
    let path = StoragePaths::trace_run_summary_file(run_id);
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn get_run_overview(run_id: &str) -> Option<RunOverview> {
    let summary = get_run(run_id)?;
    let records = get_records(run_id, &RecordQuery::default());
    let spans = collect_span_summaries(&records);
    let mut turn_ids = HashSet::new();
    let mut iteration_ids = HashSet::new();
    let mut context = RunContextOverview::default();

    for record in &records {
        if let Some(turn_id) = record.turn_id.as_ref() {
            turn_ids.insert(turn_id.clone());
        }

        if let Some(iteration) = record.iteration {
            iteration_ids.insert((record.turn_id.clone().unwrap_or_default(), iteration));
        }

        if matches!(record.actor, super::model::TraceActor::Context)
            || matches!(
                record.name.as_str(),
                "plan_updated"
                    | "task_state_changed"
                    | "context_compacted"
                    | "tool_result_truncated"
                    | "yielded_to_user"
            )
        {
            context.total_events += 1;
        }

        match record.name.as_str() {
            "plan_updated" => context.plan_updates += 1,
            "task_state_changed" => context.task_state_updates += 1,
            "context_compacted" => context.compactions += 1,
            "tool_result_truncated" => context.truncations += 1,
            "yielded_to_user" => context.yields += 1,
            _ => {}
        }
    }

    let mut slowest_spans: Vec<_> = spans
        .iter()
        .filter(|span| is_actionable_span(span))
        .cloned()
        .collect();
    slowest_spans.sort_by(|left, right| {
        right
            .duration_ms
            .cmp(&left.duration_ms)
            .then_with(|| left.started_at_unix_ms.cmp(&right.started_at_unix_ms))
    });
    slowest_spans.truncate(8);

    let mut hotspots = collect_hotspots(
        &spans
            .iter()
            .filter(|span| is_actionable_span(span))
            .cloned()
            .collect::<Vec<_>>(),
    );
    hotspots.sort_by(|left, right| {
        right
            .total_duration_ms
            .cmp(&left.total_duration_ms)
            .then_with(|| right.max_duration_ms.cmp(&left.max_duration_ms))
            .then_with(|| left.name.cmp(&right.name))
    });
    hotspots.truncate(6);

    Some(RunOverview {
        summary,
        turn_count: turn_ids.len() as u64,
        iteration_count: iteration_ids.len() as u64,
        context,
        slowest_spans,
        hotspots,
    })
}

pub fn get_records(run_id: &str, query: &RecordQuery) -> Vec<TraceRecord> {
    let mut records = super::sqlite::get_records(run_id).unwrap_or_default();
    if records.is_empty() {
        records = json_records(run_id);
    }
    records.retain(|record| record_matches(record, query));
    records.sort_by_key(|record| record.ts_unix_ms);
    records
}

fn json_records(run_id: &str) -> Vec<TraceRecord> {
    let path = StoragePaths::trace_run_records_file(run_id);
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };

    let mut records = Vec::new();
    for line in content.lines() {
        let Ok(record) = serde_json::from_str::<TraceRecord>(line) else {
            continue;
        };

        records.push(record);
    }
    records
}

pub fn get_artifacts(run_id: &str) -> TraceArtifacts {
    let records = get_records(run_id, &RecordQuery::default());
    let mut files = Vec::new();
    let mut evidence = Vec::new();
    let mut subagent_sessions = Vec::new();

    for record in records {
        if let Some(path) = record.attrs.get("file_path").and_then(Value::as_str) {
            if !files.iter().any(|value| value == path) {
                files.push(path.to_string());
            }
        }

        if let Some(kind) = record.attrs.get("evidence_kind").and_then(Value::as_str) {
            evidence.push(serde_json::json!({
                "kind": kind,
                "source_path": record.attrs.get("evidence_source_path"),
                "summary": record.attrs.get("evidence_summary"),
            }));
        }

        if record.attrs.get("sub_session_id").is_some() {
            subagent_sessions.push(serde_json::json!({
                "sub_session_id": record.attrs.get("sub_session_id"),
                "job_id": record.attrs.get("job_id"),
                "transcript_path": record.attrs.get("transcript_path"),
                "event_log_path": record.attrs.get("event_log_path"),
            }));
        }
    }

    TraceArtifacts {
        files,
        evidence,
        subagent_sessions,
    }
}

pub fn get_tree(run_id: &str) -> Vec<TraceTreeNode> {
    let records = get_records(run_id, &RecordQuery::default());
    let mut start_records = HashMap::<String, TraceRecord>::new();
    let mut end_records = HashMap::<String, TraceRecord>::new();

    for record in records {
        if let Some(span_id) = &record.span_id {
            match record.kind {
                super::model::TraceKind::SpanStart => {
                    start_records.insert(span_id.clone(), record);
                }
                super::model::TraceKind::SpanEnd => {
                    end_records.insert(span_id.clone(), record);
                }
                super::model::TraceKind::Event => {}
            }
        }
    }

    let mut nodes = HashMap::<String, TraceTreeNode>::new();
    let mut children_by_parent = HashMap::<Option<String>, Vec<String>>::new();
    for (span_id, start) in &start_records {
        let end = end_records.get(span_id);
        let node = TraceTreeNode {
            span_id: span_id.clone(),
            parent_span_id: start.parent_span_id.clone(),
            actor: start.actor.clone(),
            name: start.name.clone(),
            status: end
                .map(|record| record.status.as_str().to_string())
                .unwrap_or_else(|| "running".to_string()),
            started_at_unix_ms: start.ts_unix_ms,
            duration_ms: end.and_then(|record| record.duration_ms),
            summary: end
                .and_then(|record| record.summary.clone())
                .or_else(|| start.summary.clone()),
            attrs: start.attrs.clone(),
            children: Vec::new(),
        };
        children_by_parent
            .entry(start.parent_span_id.clone())
            .or_default()
            .push(span_id.clone());
        nodes.insert(span_id.clone(), node);
    }

    let mut root_ids = children_by_parent.remove(&None).unwrap_or_default();
    for node in nodes.values() {
        if let Some(parent_span_id) = &node.parent_span_id {
            if !nodes.contains_key(parent_span_id) && !root_ids.contains(&node.span_id) {
                root_ids.push(node.span_id.clone());
            }
        }
    }
    let mut roots: Vec<TraceTreeNode> = root_ids
        .into_iter()
        .filter_map(|span_id| build_tree_node(&span_id, &nodes, &children_by_parent))
        .collect();

    roots.sort_by_key(|node| node.started_at_unix_ms);
    roots
}

pub fn find_run_for_subsession(
    parent_session_id: &str,
    sub_session_id: &str,
) -> Option<RunSummary> {
    let runs = list_runs(&RunQuery {
        session_id: Some(parent_session_id.to_string()),
        ..RunQuery::default()
    });
    for run in runs {
        let records = get_records(&run.run_id, &RecordQuery::default());
        if records.iter().any(|record| {
            record.session_id == sub_session_id
                || record.attrs.get("sub_session_id").and_then(Value::as_str)
                    == Some(sub_session_id)
        }) {
            return Some(run);
        }
    }
    None
}

fn build_tree_node(
    span_id: &str,
    nodes: &HashMap<String, TraceTreeNode>,
    children_by_parent: &HashMap<Option<String>, Vec<String>>,
) -> Option<TraceTreeNode> {
    let mut node = nodes.get(span_id)?.clone();
    let child_ids = children_by_parent
        .get(&Some(span_id.to_string()))
        .cloned()
        .unwrap_or_default();
    let mut children = Vec::new();
    for child_id in child_ids {
        if let Some(child) = build_tree_node(&child_id, nodes, children_by_parent) {
            children.push(child);
        }
    }
    children.sort_by_key(|child| child.started_at_unix_ms);
    node.children = children;
    Some(node)
}

fn sqlite_or_json_runs() -> Vec<RunSummary> {
    let mut merged = HashMap::new();
    if let Some(runs) = super::sqlite::list_runs() {
        for run in runs {
            merged.insert(run.run_id.clone(), run);
        }
    }
    for run in json_runs() {
        merged.entry(run.run_id.clone()).or_insert(run);
    }
    merged.into_values().collect()
}

fn json_runs() -> Vec<RunSummary> {
    let dir = StoragePaths::trace_runs_dir();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut runs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(summary) = serde_json::from_str::<RunSummary>(&content) else {
            continue;
        };
        runs.push(summary);
    }
    runs
}

fn run_matches(summary: &RunSummary, query: &RunQuery) -> bool {
    if let Some(session_id) = &query.session_id {
        if &summary.session_id != session_id && &summary.root_session_id != session_id {
            return false;
        }
    }

    if let Some(task_id) = &query.task_id {
        if summary.task_id.as_deref() != Some(task_id.as_str()) {
            return false;
        }
    }

    if let Some(status) = &query.status {
        if &summary.status != status {
            return false;
        }
    }

    if let Some(tool_name) = &query.tool_name {
        if !summary.tool_names.iter().any(|name| name == tool_name) {
            return false;
        }
    }

    if let Some(from_unix_ms) = query.from_unix_ms {
        if summary.started_at_unix_ms < from_unix_ms {
            return false;
        }
    }

    if let Some(to_unix_ms) = query.to_unix_ms {
        if summary.started_at_unix_ms > to_unix_ms {
            return false;
        }
    }

    if let Some(min_duration_ms) = query.min_duration_ms {
        let duration_ms = summary
            .duration_ms
            .unwrap_or_else(|| super::unix_ms_now().saturating_sub(summary.started_at_unix_ms));
        if duration_ms < min_duration_ms {
            return false;
        }
    }

    if let Some(text) = &query.query {
        let needle = text.to_lowercase();
        let mut haystacks = vec![
            summary.run_id.to_lowercase(),
            summary.session_id.to_lowercase(),
            summary.root_session_id.to_lowercase(),
            summary
                .root_goal
                .as_deref()
                .unwrap_or_default()
                .to_lowercase(),
            summary
                .last_error_summary
                .as_deref()
                .unwrap_or_default()
                .to_lowercase(),
            summary
                .provider
                .as_deref()
                .unwrap_or_default()
                .to_lowercase(),
            summary.model.as_deref().unwrap_or_default().to_lowercase(),
            summary
                .task_id
                .as_deref()
                .unwrap_or_default()
                .to_lowercase(),
        ];
        haystacks.extend(summary.tool_names.iter().map(|name| name.to_lowercase()));

        if !haystacks.iter().any(|value| value.contains(&needle)) {
            return false;
        }
    }

    true
}

fn collect_span_summaries(records: &[TraceRecord]) -> Vec<TraceSpanSummary> {
    let mut start_records = HashMap::<String, &TraceRecord>::new();
    let mut end_records = HashMap::<String, &TraceRecord>::new();
    let now = super::unix_ms_now();

    for record in records {
        let Some(span_id) = record.span_id.as_ref() else {
            continue;
        };

        match record.kind {
            super::model::TraceKind::SpanStart => {
                start_records.entry(span_id.clone()).or_insert(record);
            }
            super::model::TraceKind::SpanEnd => {
                end_records.insert(span_id.clone(), record);
            }
            super::model::TraceKind::Event => {}
        }
    }

    let mut spans = Vec::new();
    for (span_id, start) in start_records {
        let end = end_records.get(&span_id).copied();
        let duration_ms = end
            .and_then(|record| record.duration_ms)
            .unwrap_or_else(|| match end {
                Some(record) => record.ts_unix_ms.saturating_sub(start.ts_unix_ms),
                None => now.saturating_sub(start.ts_unix_ms),
            });
        spans.push(TraceSpanSummary {
            span_id,
            parent_span_id: start.parent_span_id.clone(),
            actor: start.actor.clone(),
            name: start.name.clone(),
            status: end
                .map(|record| record.status.as_str().to_string())
                .unwrap_or_else(|| "running".to_string()),
            started_at_unix_ms: start.ts_unix_ms,
            duration_ms,
            turn_id: end
                .and_then(|record| record.turn_id.clone())
                .or_else(|| start.turn_id.clone()),
            iteration: end.and_then(|record| record.iteration).or(start.iteration),
            summary: end
                .and_then(|record| record.summary.clone())
                .or_else(|| start.summary.clone()),
        });
    }
    spans
}

fn collect_hotspots(spans: &[TraceSpanSummary]) -> Vec<TraceHotspot> {
    let mut grouped = HashMap::<(String, String), TraceHotspot>::new();

    for span in spans {
        let key = (span.actor.as_str().to_string(), span.name.clone());
        let hotspot = grouped.entry(key).or_insert_with(|| TraceHotspot {
            actor: span.actor.clone(),
            name: span.name.clone(),
            count: 0,
            total_duration_ms: 0,
            max_duration_ms: 0,
            error_count: 0,
        });
        hotspot.count += 1;
        hotspot.total_duration_ms += span.duration_ms;
        hotspot.max_duration_ms = hotspot.max_duration_ms.max(span.duration_ms);
        if matches!(span.status.as_str(), "error" | "timed_out" | "cancelled") {
            hotspot.error_count += 1;
        }
    }

    grouped.into_values().collect()
}

fn is_actionable_span(span: &TraceSpanSummary) -> bool {
    !matches!(
        span.name.as_str(),
        "run_started" | "turn_started" | "iteration_started"
    )
}

fn record_matches(record: &TraceRecord, query: &RecordQuery) -> bool {
    if let Some(actor) = &query.actor {
        if record.actor.as_str() != actor {
            return false;
        }
    }

    if let Some(name) = &query.name {
        if &record.name != name {
            return false;
        }
    }

    if let Some(status) = &query.status {
        if record.status.as_str() != status {
            return false;
        }
    }

    if let Some(turn_id) = &query.turn_id {
        if record.turn_id.as_deref() != Some(turn_id.as_str()) {
            return false;
        }
    }

    if let Some(iteration) = query.iteration {
        if record.iteration != Some(iteration) {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::CURRENT_SCHEMA_VERSION;
    use crate::trace::{TraceActor, TraceKind, TraceLevel, TraceStatus};
    use serial_test::serial;

    fn unique_run_id(prefix: &str) -> String {
        format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
    }

    fn write_summary(summary: &RunSummary) {
        let path = StoragePaths::trace_run_summary_file(&summary.run_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, serde_json::to_string(summary).unwrap()).unwrap();
    }

    fn write_records(run_id: &str, records: &[TraceRecord]) {
        let path = StoragePaths::trace_run_records_file(run_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let body = records
            .iter()
            .map(|record| serde_json::to_string(record).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(path, format!("{body}\n")).unwrap();
    }

    struct RecordParams<'a> {
        run_id: &'a str,
        session_id: &'a str,
        parent_span_id: Option<&'a str>,
        span_id: Option<&'a str>,
        kind: TraceKind,
        name: &'a str,
        actor: TraceActor,
        status: TraceStatus,
        ts_unix_ms: u64,
    }

    fn record(params: RecordParams) -> TraceRecord {
        TraceRecord {
            schema_version: CURRENT_SCHEMA_VERSION,
            record_id: format!("rec_{}_{}", params.name, params.ts_unix_ms),
            trace_id: params.run_id.to_string(),
            run_id: params.run_id.to_string(),
            span_id: params.span_id.map(str::to_string),
            parent_span_id: params.parent_span_id.map(str::to_string),
            session_id: params.session_id.to_string(),
            task_id: None,
            turn_id: None,
            iteration: None,
            actor: params.actor,
            kind: params.kind,
            name: params.name.to_string(),
            status: params.status,
            ts_unix_ms: params.ts_unix_ms,
            duration_ms: None,
            level: TraceLevel::Normal,
            summary: None,
            attrs: serde_json::json!({}),
        }
    }

    fn cleanup(run_id: &str) {
        let _ = std::fs::remove_file(StoragePaths::trace_run_summary_file(run_id));
        let _ = std::fs::remove_file(StoragePaths::trace_run_records_file(run_id));
    }

    #[test]
    #[serial]
    fn get_tree_keeps_grandchildren_nested() {
        let run_id = unique_run_id("trace_tree_nested_test");
        cleanup(&run_id);

        write_summary(&RunSummary::new(&run_id, &run_id, "sess_tree", "sess_tree"));
        write_records(
            &run_id,
            &[
                record(RecordParams {
                    run_id: &run_id,
                    session_id: "sess_tree",
                    parent_span_id: None,
                    span_id: Some("root"),
                    kind: TraceKind::SpanStart,
                    name: "run_started",
                    actor: TraceActor::MainAgent,
                    status: TraceStatus::Running,
                    ts_unix_ms: 1,
                }),
                record(RecordParams {
                    run_id: &run_id,
                    session_id: "sess_tree",
                    parent_span_id: Some("root"),
                    span_id: Some("child"),
                    kind: TraceKind::SpanStart,
                    name: "iteration_started",
                    actor: TraceActor::MainAgent,
                    status: TraceStatus::Running,
                    ts_unix_ms: 2,
                }),
                record(RecordParams {
                    run_id: &run_id,
                    session_id: "sess_tree",
                    parent_span_id: Some("child"),
                    span_id: Some("grandchild"),
                    kind: TraceKind::SpanStart,
                    name: "tool_started",
                    actor: TraceActor::Tool,
                    status: TraceStatus::Running,
                    ts_unix_ms: 3,
                }),
            ],
        );

        let tree = get_tree(&run_id);

        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].span_id, "root");
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(tree[0].children[0].span_id, "child");
        assert_eq!(tree[0].children[0].children.len(), 1);
        assert_eq!(tree[0].children[0].children[0].span_id, "grandchild");

        cleanup(&run_id);
    }

    #[test]
    #[serial]
    fn find_run_for_subsession_returns_parent_run() {
        let run_id = unique_run_id("trace_find_subsession_test");
        cleanup(&run_id);

        let mut summary = RunSummary::new(&run_id, &run_id, "parent_session", "parent_session");
        summary.started_at_unix_ms = 10;
        write_summary(&summary);
        write_records(
            &run_id,
            &[TraceRecord {
                schema_version: CURRENT_SCHEMA_VERSION,
                record_id: "rec_sub".to_string(),
                trace_id: run_id.clone(),
                run_id: run_id.clone(),
                span_id: None,
                parent_span_id: None,
                session_id: "sub_session_1".to_string(),
                task_id: None,
                turn_id: None,
                iteration: None,
                actor: TraceActor::Subagent,
                kind: TraceKind::Event,
                name: "subagent_finished".to_string(),
                status: TraceStatus::Ok,
                ts_unix_ms: 11,
                duration_ms: None,
                level: TraceLevel::Normal,
                summary: Some("done".to_string()),
                attrs: serde_json::json!({
                    "root_session_id": "parent_session",
                    "sub_session_id": "sub_session_1",
                }),
            }],
        );

        let found = find_run_for_subsession("parent_session", "sub_session_1")
            .expect("expected to find run");

        assert_eq!(found.run_id, run_id);

        cleanup(&run_id);
    }

    #[test]
    #[serial]
    fn list_runs_filters_by_task_and_min_duration() {
        let run_id = unique_run_id("trace_query_task_filter_test");
        cleanup(&run_id);

        let mut summary = RunSummary::new(&run_id, &run_id, "sess_filter", "sess_filter");
        summary.task_id = Some("task_trace_query_unique_123".to_string());
        summary.duration_ms = Some(900);
        summary.started_at_unix_ms = 20;
        write_summary(&summary);

        let hits = list_runs(&RunQuery {
            task_id: Some("task_trace_query_unique_123".to_string()),
            min_duration_ms: Some(500),
            ..RunQuery::default()
        });
        assert!(hits.iter().any(|item| item.run_id == run_id));

        let misses = list_runs(&RunQuery {
            task_id: Some("task_trace_query_unique_123".to_string()),
            min_duration_ms: Some(1500),
            ..RunQuery::default()
        });
        assert!(!misses.iter().any(|item| item.run_id == run_id));

        cleanup(&run_id);
    }

    #[test]
    #[serial]
    fn get_run_overview_summarizes_context_and_slowest_spans() {
        let run_id = unique_run_id("trace_overview_test");
        cleanup(&run_id);

        let mut summary = RunSummary::new(&run_id, &run_id, "sess_overview", "sess_overview");
        summary.started_at_unix_ms = 100;
        summary.duration_ms = Some(400);
        write_summary(&summary);
        write_records(
            &run_id,
            &[
                TraceRecord {
                    turn_id: Some("turn_a".to_string()),
                    iteration: Some(1),
                    summary: Some("run".to_string()),
                    ..record(RecordParams {
                        run_id: &run_id,
                        session_id: "sess_overview",
                        parent_span_id: None,
                        span_id: Some("run_span"),
                        kind: TraceKind::SpanStart,
                        name: "run_started",
                        actor: TraceActor::MainAgent,
                        status: TraceStatus::Running,
                        ts_unix_ms: 100,
                    })
                },
                TraceRecord {
                    turn_id: Some("turn_a".to_string()),
                    iteration: Some(1),
                    ..record(RecordParams {
                        run_id: &run_id,
                        session_id: "sess_overview",
                        parent_span_id: Some("run_span"),
                        span_id: Some("tool_span"),
                        kind: TraceKind::SpanStart,
                        name: "tool_started",
                        actor: TraceActor::Tool,
                        status: TraceStatus::Running,
                        ts_unix_ms: 120,
                    })
                },
                TraceRecord {
                    turn_id: Some("turn_a".to_string()),
                    iteration: Some(1),
                    duration_ms: Some(240),
                    summary: Some("write_file".to_string()),
                    attrs: serde_json::json!({ "tool_name": "write_file" }),
                    ..record(RecordParams {
                        run_id: &run_id,
                        session_id: "sess_overview",
                        parent_span_id: Some("run_span"),
                        span_id: Some("tool_span"),
                        kind: TraceKind::SpanEnd,
                        name: "tool_finished",
                        actor: TraceActor::Tool,
                        status: TraceStatus::Ok,
                        ts_unix_ms: 360,
                    })
                },
                TraceRecord {
                    turn_id: Some("turn_a".to_string()),
                    iteration: Some(1),
                    ..record(RecordParams {
                        run_id: &run_id,
                        session_id: "sess_overview",
                        parent_span_id: Some("run_span"),
                        span_id: None,
                        kind: TraceKind::Event,
                        name: "plan_updated",
                        actor: TraceActor::Context,
                        status: TraceStatus::Ok,
                        ts_unix_ms: 200,
                    })
                },
                TraceRecord {
                    turn_id: Some("turn_a".to_string()),
                    iteration: Some(1),
                    ..record(RecordParams {
                        run_id: &run_id,
                        session_id: "sess_overview",
                        parent_span_id: Some("run_span"),
                        span_id: None,
                        kind: TraceKind::Event,
                        name: "context_compacted",
                        actor: TraceActor::Context,
                        status: TraceStatus::Ok,
                        ts_unix_ms: 210,
                    })
                },
                TraceRecord {
                    turn_id: Some("turn_a".to_string()),
                    iteration: Some(1),
                    ..record(RecordParams {
                        run_id: &run_id,
                        session_id: "sess_overview",
                        parent_span_id: Some("run_span"),
                        span_id: None,
                        kind: TraceKind::Event,
                        name: "yielded_to_user",
                        actor: TraceActor::MainAgent,
                        status: TraceStatus::Yielded,
                        ts_unix_ms: 220,
                    })
                },
            ],
        );

        let overview = get_run_overview(&run_id).expect("expected overview");

        assert_eq!(overview.turn_count, 1);
        assert_eq!(overview.iteration_count, 1);
        assert_eq!(overview.context.plan_updates, 1);
        assert_eq!(overview.context.compactions, 1);
        assert_eq!(overview.context.yields, 1);
        assert_eq!(overview.slowest_spans[0].span_id, "tool_span");
        assert_eq!(overview.slowest_spans[0].duration_ms, 240);
        assert!(overview
            .hotspots
            .iter()
            .any(|item| item.name == "tool_started" && item.total_duration_ms == 240));

        cleanup(&run_id);
    }
}
