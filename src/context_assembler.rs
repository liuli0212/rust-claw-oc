use crate::context::DetailedContextStats;

use std::fs;
use std::path::Path;
use tiktoken_rs::CoreBPE;

pub struct AssemblyInputs<'a> {
    pub system_prompts: &'a [String],
    pub transcript_path: Option<&'a Path>,
    pub task_state_summary: Option<&'a str>,
    pub legacy_task_plan: Option<&'a str>,
    pub retrieved_memory: Option<&'a str>,
    pub evidence_candidates: Vec<PromptCandidate>,
    pub max_history_tokens: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssemblyReport {
    pub evicted_item_labels: Vec<String>,
    pub stale_evidence_refreshes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCandidate {
    pub id: String,
    pub kind: String,
    pub layer: String,
    pub text: String,
    pub priority_score: i32,
    pub token_cost: usize,
    pub required: bool,
    pub retrieved_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AllocationResult {
    pub included: Vec<PromptCandidate>,
    pub evicted: Vec<String>,
}

pub fn assemble_prompt_sections(
    bpe: &CoreBPE,
    inputs: AssemblyInputs<'_>,
) -> (String, DetailedContextStats, AssemblyReport) {
    let mut stats = DetailedContextStats::default();
    let mut sections = Vec::new();
    let mut report = AssemblyReport::default();

    let identity = build_prompt_section("Identity", inputs.system_prompts.join("\n\n"), 4_000);
    let runtime = build_runtime_section(inputs.transcript_path);

    if let Some(section) = identity {
        stats.system_static = bpe.encode_with_special_tokens(&section).len();
        sections.push(section);
    }
    if let Some(section) = runtime {
        stats.system_runtime = bpe.encode_with_special_tokens(&section).len();
        sections.push(section);
    }

    let mut optional_candidates = Vec::new();

    if let Ok(custom_prompt) = fs::read_to_string(".claw_prompt.md") {
        push_candidate(
            &mut optional_candidates,
            bpe,
            "custom_instructions",
            "instructions",
            "durable",
            build_prompt_section("User Custom Instructions", custom_prompt, 4_000),
            90,
            Some(1),
        );
    }

    if let Some(task_state_summary) = inputs.task_state_summary {
        push_candidate(
            &mut optional_candidates,
            bpe,
            "task_state",
            "task_state",
            "task_state",
            build_prompt_section(
                "Current Task State (Derived)",
                task_state_summary.to_string(),
                4_000,
            ),
            95,
            Some(3),
        );
    } else if let Some(legacy_task_plan) = inputs.legacy_task_plan {
        push_candidate(
            &mut optional_candidates,
            bpe,
            "legacy_task_plan",
            "task_plan",
            "task_state",
            build_prompt_section(
                "Current Task Plan (STRICT)",
                legacy_task_plan.to_string(),
                4_000,
            ),
            90,
            Some(2),
        );
    }

    let mut project_context = String::new();
    if inputs.task_state_summary.is_none() && inputs.legacy_task_plan.is_none() {
        project_context.push_str("### Task Planning\n");
        project_context.push_str("If the user request is complex (e.g. multi-step refactoring, new feature implementation), you MUST use the `task_plan` tool immediately to create a structured plan (action='add').\n\n");
    }
    if let Ok(content) = fs::read_to_string("AGENTS.md") {
        project_context.push_str("### AGENTS.md\n");
        project_context.push_str(&truncate_chars(&content, 3_000));
        project_context.push_str("\n\n");
    }
    if let Ok(content) = fs::read_to_string("README.md") {
        project_context.push_str("### README.md\n");
        project_context.push_str(&truncate_chars(&content, 2_500));
        project_context.push_str("\n\n");
    }
    if let Ok(content) = fs::read_to_string("MEMORY.md") {
        project_context.push_str("### MEMORY.md\n");
        project_context.push_str(&truncate_chars(&content, 1_500));
        project_context.push_str("\n\n");
    }
    push_candidate(
        &mut optional_candidates,
        bpe,
        "project_context",
        "project_context",
        "durable",
        build_prompt_section("Project Context", project_context, 7_000),
        50,
        Some(1),
    );

    optional_candidates.extend(inputs.evidence_candidates);

    if let Some(memory) = inputs.retrieved_memory {
        push_candidate(
            &mut optional_candidates,
            bpe,
            "retrieved_memory",
            "memory",
            "evidence",
            build_prompt_section("Retrieved Memory", memory.to_string(), 3_000),
            80,
            Some(2),
        );
    }

    let required_cost = stats.system_static + stats.system_runtime;
    let optional_budget = system_optional_budget(inputs.max_history_tokens, required_cost);
    let allocation = allocate_candidates(optional_budget, Vec::new(), optional_candidates);

    for candidate in allocation.included {
        match candidate.id.as_str() {
            "custom_instructions" => stats.system_custom = candidate.token_cost,
            "task_state" | "legacy_task_plan" => stats.system_task_plan = candidate.token_cost,
            "project_context" => stats.system_project = candidate.token_cost,
            _ if candidate.layer == "evidence" => stats.memory += candidate.token_cost,
            _ => {}
        }
        sections.push(candidate.text);
    }

    if !allocation.evicted.is_empty() {
        report.evicted_item_labels = allocation.evicted.clone();
        sections.push(format!(
            "## Context Budget Notes\n{}\n",
            allocation.evicted.join("\n")
        ));
    }

    stats.max = inputs.max_history_tokens;
    (sections.join("\n"), stats, report)
}

pub fn allocate_candidates(
    budget: usize,
    required: Vec<PromptCandidate>,
    optional: Vec<PromptCandidate>,
) -> AllocationResult {
    let mut result = AllocationResult::default();
    let mut used = 0usize;

    for candidate in required {
        used = used.saturating_add(candidate.token_cost);
        result.included.push(candidate);
    }

    let mut optional = optional;
    optional.sort_by(|left, right| {
        right
            .priority_score
            .cmp(&left.priority_score)
            .then_with(|| right.retrieved_at.cmp(&left.retrieved_at))
            .then_with(|| left.id.cmp(&right.id))
    });

    for candidate in optional {
        if used + candidate.token_cost <= budget {
            used += candidate.token_cost;
            result.included.push(candidate);
        } else {
            result.evicted.push(format!(
                "[Context item '{}' evicted due to context budget]",
                candidate.id
            ));
        }
    }

    result
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    input.chars().take(max_chars).collect()
}

fn build_prompt_section(title: &str, content: String, max_chars: usize) -> Option<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    let truncated = truncate_chars(trimmed, max_chars);
    Some(format!("## {title}\n{truncated}\n"))
}

fn build_runtime_section(transcript_path: Option<&Path>) -> Option<String> {
    let mut runtime = String::new();
    runtime.push_str(&format!("OS: {}\n", std::env::consts::OS));
    runtime.push_str(&format!("Architecture: {}\n", std::env::consts::ARCH));
    if let Ok(dir) = std::env::current_dir() {
        runtime.push_str(&format!("Current Directory: {}\n", dir.display()));
    }
    if let Some(path) = transcript_path {
        runtime.push_str(&format!("Session Transcript: {}\n", path.display()));
    }
    build_prompt_section("Runtime Environment", runtime, 1_000)
}

fn push_candidate(
    out: &mut Vec<PromptCandidate>,
    bpe: &CoreBPE,
    id: &str,
    kind: &str,
    layer: &str,
    section: Option<String>,
    priority_score: i32,
    retrieved_at: Option<u64>,
) {
    let Some(text) = section else {
        return;
    };
    let token_cost = bpe.encode_with_special_tokens(&text).len();
    out.push(PromptCandidate {
        id: id.to_string(),
        kind: kind.to_string(),
        layer: layer.to_string(),
        text,
        priority_score,
        token_cost,
        required: false,
        retrieved_at,
    });
}

fn system_optional_budget(max_history_tokens: usize, required_cost: usize) -> usize {
    let cap = (max_history_tokens / 8).clamp(512, 4096);
    cap.saturating_sub(required_cost.min(cap / 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_assemble_prompt_sections_prefers_task_state() {
        let bpe = tiktoken_rs::cl100k_base().unwrap();
        let (prompt, stats, _report) = assemble_prompt_sections(
            &bpe,
            AssemblyInputs {
                system_prompts: &["Identity".to_string()],
                transcript_path: None,
                task_state_summary: Some("Status: in_progress\nGoal: Test"),
                legacy_task_plan: Some("legacy plan"),
                retrieved_memory: None,
                evidence_candidates: Vec::new(),
                max_history_tokens: 100,
            },
        );

        assert!(prompt.contains("Current Task State (Derived)"));
        assert!(!prompt.contains("Current Task Plan (STRICT)"));
        assert!(stats.system_task_plan > 0);
    }

    #[test]
    fn test_allocate_candidates_is_deterministic() {
        let result = allocate_candidates(
            10,
            vec![PromptCandidate {
                id: "required".to_string(),
                kind: "required".to_string(),
                layer: "durable".to_string(),
                text: "required".to_string(),
                priority_score: 100,
                token_cost: 3,
                required: true,
                retrieved_at: Some(1),
            }],
            vec![
                PromptCandidate {
                    id: "b".to_string(),
                    kind: "evidence".to_string(),
                    layer: "evidence".to_string(),
                    text: "b".to_string(),
                    priority_score: 5,
                    token_cost: 4,
                    required: false,
                    retrieved_at: Some(1),
                },
                PromptCandidate {
                    id: "a".to_string(),
                    kind: "evidence".to_string(),
                    layer: "evidence".to_string(),
                    text: "a".to_string(),
                    priority_score: 5,
                    token_cost: 4,
                    required: false,
                    retrieved_at: Some(2),
                },
                PromptCandidate {
                    id: "c".to_string(),
                    kind: "evidence".to_string(),
                    layer: "evidence".to_string(),
                    text: "c".to_string(),
                    priority_score: 1,
                    token_cost: 4,
                    required: false,
                    retrieved_at: Some(3),
                },
            ],
        );

        assert_eq!(result.included.len(), 2);
        assert_eq!(result.included[1].id, "a");
        assert_eq!(result.evicted.len(), 2);
        assert!(result.evicted[0].contains("b"));
    }

    #[test]
    fn test_assemble_prompt_sections_emits_budget_notes_for_evictions() {
        let bpe = tiktoken_rs::cl100k_base().unwrap();
        let huge_memory = "x".repeat(20_000);
        let (prompt, _stats, _report) = assemble_prompt_sections(
            &bpe,
            AssemblyInputs {
                system_prompts: &["Identity".to_string()],
                transcript_path: None,
                task_state_summary: Some("Status: in_progress\nGoal: Test"),
                legacy_task_plan: None,
                retrieved_memory: Some(&huge_memory),
                evidence_candidates: Vec::new(),
                max_history_tokens: 1024,
            },
        );

        assert!(prompt.contains("Context Budget Notes"));
        assert!(
            prompt.contains("retrieved_memory") || prompt.contains("project_context"),
            "expected an evicted optional section tombstone, got: {prompt}"
        );
    }
}
