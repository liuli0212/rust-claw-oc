use crate::artifact_store::{ArtifactMetadata, CleanupStats};
use crate::context::PromptReport;
use tracing::{info, info_span, Span};

pub fn agent_turn_span(session_id: &str, task_id: &str) -> Span {
    info_span!("agent.turn", session_id = session_id, task_id = task_id)
}

pub fn tool_execution_span(session_id: &str, task_id: &str, tool_name: &str) -> Span {
    info_span!(
        "tool.execute",
        session_id = session_id,
        task_id = task_id,
        tool_name = tool_name
    )
}

pub fn context_assemble_span(session_id: &str) -> Span {
    info_span!("context.assemble", session_id = session_id)
}

pub fn context_reconcile_evidence_span(evidence_count: usize) -> Span {
    info_span!(
        "context.reconcile_evidence",
        evidence_count = evidence_count
    )
}

pub fn retrieval_search_span(query: &str) -> Span {
    info_span!("retrieval.search", query = query)
}

pub fn memory_promote_span(source: &str) -> Span {
    info_span!("memory.promote", source = source)
}

pub fn record_prompt_report(session_id: &str, report: &PromptReport) {
    info!(
        target: "telemetry",
        session_id = session_id,
        total_prompt_tokens = report.total_prompt_tokens,
        cache_stable_tokens = report.cache_stable_tokens,
        volatile_tokens = report.volatile_tokens,
        system_prompt_tokens = report.system_prompt_tokens,
        history_tokens_used = report.history_tokens_used,
        current_turn_tokens = report.current_turn_tokens,
        evicted_items = report.evicted_item_count,
        evicted_item_labels = ?report.evicted_item_labels,
        stale_evidence_refreshes = report.stale_evidence_refreshes,
        "prompt_report"
    );
}

pub fn record_artifact_created(metadata: &ArtifactMetadata) {
    info!(
        target: "telemetry",
        artifact_id = metadata.artifact_id,
        session_id = metadata.session_id,
        task_id = metadata.task_id,
        source_tool = metadata.source_tool,
        is_truncated = metadata.is_truncated,
        "artifact_created"
    );
}

pub fn record_artifact_cleanup(stats: &CleanupStats) {
    info!(
        target: "telemetry",
        deleted_runs = stats.deleted_runs,
        deleted_bytes = stats.deleted_bytes,
        "artifact_cleanup"
    );
}

pub fn record_compaction_decision(turns_compacted: usize, target_tokens: usize, reason: &str) {
    info!(
        target: "telemetry",
        turns_compacted = turns_compacted,
        target_tokens = target_tokens,
        reason = reason,
        "history_compaction"
    );
}
