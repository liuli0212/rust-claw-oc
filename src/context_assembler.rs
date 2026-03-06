
use crate::evidence::Evidence;
use crate::task_state::TaskStateSnapshot;

/// Candidate for insertion into the prompt.
#[derive(Debug, Clone)]
pub struct PromptCandidate {
    pub id: String,
    pub kind: CandidateKind,
    pub priority_score: f32, // Higher is better
    pub token_cost: usize,
    pub layer: u8, // L0..L4 (0 is RunContext/System, 4 is Transcript tail)
    pub required: bool,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateKind {
    SystemInstruction,
    DurableMemory,
    ToolSchema,
    TaskStateSummary,
    Evidence(String), // Evidence ID
    VolatileTurn(usize), // Turn Index
    RunContext,
}

#[derive(Debug, Default, Clone)]
pub struct AssemblyReport {
    pub max_tokens: usize,
    pub used_tokens: usize,
    pub stable_tokens: usize,
    pub volatile_tokens: usize,
    pub total_candidates: usize,
    pub evicted_items: Vec<String>,
    pub refreshed_evidence: usize,
}

pub struct ContextAssembler {
    pub budget: usize,
}

impl ContextAssembler {
    pub fn new(budget: usize) -> Self {
        Self { budget }
    }

    /// Primary entry point for constructing a deterministic cache-aware prompt.
    pub fn assemble_prompt(
        &self,
        system_static: &str,
        tool_schemas: &str,
        durable_memory: Option<&str>,
        task_state: &TaskStateSnapshot,
        mut active_evidence: Vec<Evidence>,
        transcript_tail: Vec<String>,
    ) -> (String, AssemblyReport) {
        let mut report = AssemblyReport { max_tokens: self.budget, ..Default::default() };
        let mut candidates = Vec::new();

        // Layer 0: System and Rules (Most Stable, High Priority)
        candidates.push(PromptCandidate {
            id: "sys_static".to_string(),
            kind: CandidateKind::SystemInstruction,
            priority_score: 1000.0,
            token_cost: Self::est_tokens(system_static),
            layer: 0,
            required: true,
            content: system_static.to_string(),
        });
        candidates.push(PromptCandidate {
            id: "tools".to_string(),
            kind: CandidateKind::ToolSchema,
            priority_score: 900.0,
            token_cost: Self::est_tokens(tool_schemas),
            layer: 0,
            required: true,
            content: format!("TOOLS AVAILABLE:\n{}", tool_schemas),
        });

        // Layer 1: Durable Memory
        if let Some(mem) = durable_memory {
            if !mem.trim().is_empty() {
                candidates.push(PromptCandidate {
                    id: "durable_memory".to_string(),
                    kind: CandidateKind::DurableMemory,
                    priority_score: 800.0,
                    token_cost: Self::est_tokens(mem),
                    layer: 1,
                    required: false,
                    content: format!("WORKSPACE MEMORY:\n{}", mem),
                });
            }
        }

        // Layer 2: Reconcile and add Evidence
        for ev in &mut active_evidence {
            let (is_fresh, tombstone) = ev.is_fresh();
            if is_fresh {
                let text = format!("--- EVIDENCE ({}) ---\n{}", ev.source_path, ev.content);
                candidates.push(PromptCandidate {
                    id: ev.evidence_id.clone(),
                    kind: CandidateKind::Evidence(ev.evidence_id.clone()),
                    // Tie-breaker setup: we use retrieved_at when we sort, but for now we encode score
                    priority_score: ev.score, 
                    token_cost: Self::est_tokens(&text),
                    layer: 2,
                    required: false,
                    content: text,
                });
            } else {
                report.refreshed_evidence += 1;
                // Add tombstone
                candidates.push(PromptCandidate {
                    id: ev.evidence_id.clone(),
                    kind: CandidateKind::Evidence(ev.evidence_id.clone()),
                    priority_score: 999.0, // tombstones are tiny and critical to inform the agent
                    token_cost: 20,
                    layer: 2,
                    required: false,
                    content: tombstone.unwrap_or_default(),
                });
            }
        }

        // Layer 3: Task State
        let state_summary = task_state.summary();
        candidates.push(PromptCandidate {
            id: "task_state".to_string(),
            kind: CandidateKind::TaskStateSummary,
            priority_score: 500.0, // usually we want this over old transcript
            token_cost: Self::est_tokens(&state_summary),
            layer: 3,
            required: true, // Should never really drop the task summary unless absolutely starved
            content: format!("TASK STATE:\n{}", state_summary),
        });

        // Layer 4: Volatile Transcript Tail
        for (i, turn) in transcript_tail.into_iter().enumerate() {
            candidates.push(PromptCandidate {
                id: format!("turn_{}", i),
                kind: CandidateKind::VolatileTurn(i),
                // Recent turns are higher priority than older ones
                priority_score: 100.0 + (i as f32), 
                token_cost: Self::est_tokens(&turn),
                layer: 4,
                required: false,
                content: turn,
            });
        }

        report.total_candidates = candidates.len();

        // ---------------------------------------------------------
        // Deterministic Allocation & Eviction
        // ---------------------------------------------------------

        // 1. Reserve REQUIRED items first
        let mut budget_remaining = self.budget;
        let mut final_selection = Vec::new();
        
        let (required, mut optional): (Vec<_>, Vec<_>) = candidates.into_iter().partition(|c| c.required);

        for req in required {
            if req.token_cost <= budget_remaining {
                budget_remaining -= req.token_cost;
                final_selection.push(req);
            } else {
                // Catastrophic out-of-budget on required items
                budget_remaining = 0;
            }
        }

        // 2. Sort Optional items by stable deterministic priority
        // Tie-breaker rules:
        // - Priority Score (Desc)
        // - Lexical ID (Asc) (Substitute for retrieved_at in this simplifed structure to ensure no hash iteration randomness)
        optional.sort_by(|a, b| {
            if (a.priority_score - b.priority_score).abs() > f32::EPSILON {
                b.priority_score.partial_cmp(&a.priority_score).unwrap()
            } else {
                a.id.cmp(&b.id)
            }
        });

        for opt in optional {
            if opt.token_cost <= budget_remaining {
                budget_remaining -= opt.token_cost;
                final_selection.push(opt);
            } else {
                report.evicted_items.push(opt.id.clone());
                // Emit tombstone for evicted evidence
                if let CandidateKind::Evidence(_) = opt.kind {
                    let text = format!("[Evidence '{}' evicted due to context budget]", opt.id);
                    let cost = Self::est_tokens(&text);
                    if cost <= budget_remaining {
                        budget_remaining -= cost;
                        let mut tomb = opt.clone();
                        tomb.content = text;
                        final_selection.push(tomb);
                    }
                }
            }
        }

        // ---------------------------------------------------------
        // Stable-to-Volatile Output Ordering
        // ---------------------------------------------------------
        // We sort the FINAL selection by Layer (0 to 4), ensuring Prefix-Cache stability
        final_selection.sort_by_key(|c| c.layer);

        let mut final_prompt = String::new();
        for (i, item) in final_selection.iter().enumerate() {
            if i > 0 { final_prompt.push_str("\n\n"); }
            final_prompt.push_str(&item.content);
            
            report.used_tokens += item.token_cost;
            if item.layer <= 3 {
                report.stable_tokens += item.token_cost;
            } else {
                report.volatile_tokens += item.token_cost;
            }
        }

        (final_prompt, report)
    }

    /// Very rough heuristic for local prompt assembly without pulling in full tiktoken blocking paths here
    fn est_tokens(text: &str) -> usize {
        // approx 4 chars per token for english code text
        text.len() / 4 + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deterministic_eviction() {
        // Budget is extremely small, ~100 tokens
        let assembler = ContextAssembler::new(100);

        let active_evidence = vec![
            Evidence::new("ev1".into(), "memory".into(), "doc1".into(), 0.9, "summary".into(), "This is a very long piece of evidence that will take up tokens...".repeat(20)),
            Evidence::new("ev2".into(), "memory".into(), "doc2".into(), 0.1, "summary".into(), "Short evidence".into()),
        ];

        let state = TaskStateSnapshot::empty();

        let (prompt, report) = assembler.assemble_prompt(
            "System Instructions",
            "Tool Schema", 
            None,
            &state,
            active_evidence,
            vec!["Turn 1".into(), "Turn 2".into()]
        );

        // Required items should make it.
        assert!(prompt.contains("System Instructions"));
        assert!(prompt.contains("TASK STATE"));
        
        // ev1 was huge but high cost, it won't fit a 100 token budget alongside Sys and State.
        // It should be evicted deterministically.
        assert!(report.evicted_items.contains(&"ev1".to_string()));
        // Note: Tombstone should replace it if room allows.
        assert!(prompt.contains("[Evidence 'ev1' evicted due to context budget]"));
    }

    #[test]
    fn test_ordering() {
        let assembler = ContextAssembler::new(5000);
        let mut state = TaskStateSnapshot::empty();
        state.goal = Some("Test Ordering".into());

        let ev1 = Evidence::new("ev1".into(), "memory".into(), "doc1".into(), 0.9, "summary".into(), "Evidence content".into());

        let (prompt, _) = assembler.assemble_prompt(
            "SYS", "TOOLS", Some("DURABLE MEMORY"), &state, vec![ev1], vec!["LAST VOLATILE".into()]
        );

        // Verify layer sorting
        let idx_sys = prompt.find("SYS").unwrap();
        let idx_mem = prompt.find("DURABLE MEMORY").unwrap();
        let idx_ev = prompt.find("Evidence content").unwrap();
        let idx_state = prompt.find("TASK STATE").unwrap();
        let idx_vol = prompt.find("LAST VOLATILE").unwrap();

        assert!(idx_sys < idx_mem);
        assert!(idx_mem < idx_ev);
        assert!(idx_ev < idx_state);
        assert!(idx_state < idx_vol);
    }
}
