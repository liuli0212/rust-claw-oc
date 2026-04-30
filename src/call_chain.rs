use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::Value;

pub const MAX_CALL_CHAIN_CALLS_PER_ROOT_REQUEST: usize = 6;
pub const MAX_CALL_CHAIN_DEPTH: usize = 3;

#[derive(Debug, Clone)]
pub struct CallChainFrame {
    pub skill_name: String,
    pub call_id: String,
    pub parent_call_id: Option<String>,
    pub depth: usize,
    pub args_digest: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CallChainContext {
    pub lineage: Vec<CallChainFrame>,
    pub total_calls: Arc<AtomicUsize>,
    pub root_session_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct CallChainBudget {
    pub remaining_steps: Option<usize>,
    pub remaining_timeout_sec: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct CallChainSeed {
    pub inherited_context: Option<CallChainContext>,
    pub inherited_budget: CallChainBudget,
    pub handoff_context: Option<String>,
}

impl CallChainContext {
    pub fn new_root(root_session_id: impl Into<String>) -> Self {
        Self {
            lineage: Vec::new(),
            total_calls: Arc::new(AtomicUsize::new(0)),
            root_session_id: root_session_id.into(),
        }
    }

    pub fn lineage_names(&self) -> Vec<String> {
        self.lineage
            .iter()
            .map(|frame| frame.skill_name.clone())
            .collect()
    }

    pub fn contains_skill(&self, skill_name: &str) -> bool {
        self.lineage
            .iter()
            .any(|frame| frame.skill_name == skill_name)
    }

    pub fn current_depth(&self) -> usize {
        self.lineage.len()
    }

    pub fn total_calls_used(&self) -> usize {
        self.total_calls.load(Ordering::SeqCst)
    }

    pub fn append_frame(&self, skill_name: &str, args: Option<&str>) -> Self {
        let mut lineage = self.lineage.clone();
        let depth = lineage.len() + 1;
        let parent_call_id = lineage.last().map(|frame| frame.call_id.clone());
        lineage.push(CallChainFrame {
            skill_name: skill_name.to_string(),
            call_id: format!("call_{}", uuid::Uuid::new_v4().simple()),
            parent_call_id,
            depth,
            args_digest: args.map(args_digest),
        });
        Self {
            lineage,
            total_calls: self.total_calls.clone(),
            root_session_id: self.root_session_id.clone(),
        }
    }
}

pub fn args_digest(input: &str) -> String {
    let mut hasher = DefaultHasher::new();
    input.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub fn effective_limits(
    budget: &CallChainBudget,
    requested_max_steps: Option<usize>,
    requested_timeout_sec: Option<u64>,
    default_max_steps: usize,
    default_timeout_sec: u64,
) -> (usize, u64) {
    let effective_max_steps = match budget.remaining_steps.map(|steps| steps.max(1)) {
        Some(parent_remaining_steps) => {
            let requested_steps = requested_max_steps.unwrap_or(parent_remaining_steps).max(1);
            requested_steps.min((parent_remaining_steps / 2).max(1))
        }
        None => requested_max_steps.unwrap_or(default_max_steps).max(1),
    };

    let effective_timeout_sec = match budget
        .remaining_timeout_sec
        .map(|timeout_sec| timeout_sec.max(1))
    {
        Some(parent_remaining_timeout_sec) => {
            let requested_timeout_sec = requested_timeout_sec.unwrap_or(default_timeout_sec).max(1);
            requested_timeout_sec.min(parent_remaining_timeout_sec)
        }
        None => requested_timeout_sec.unwrap_or(default_timeout_sec).max(1),
    };

    (effective_max_steps, effective_timeout_sec)
}

pub fn build_skill_activation_command(
    skill_name: &str,
    raw_args: Option<&str>,
    json_args: Option<&Value>,
) -> String {
    match (json_args, raw_args) {
        (Some(json_args), _) => format!("/{skill_name} {}", json_args),
        (None, Some(raw_args)) if !raw_args.trim().is_empty() => {
            format!("/{skill_name} {}", raw_args.trim())
        }
        _ => format!("/{skill_name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_append_frame_tracks_depth_and_parent() {
        let root = CallChainContext::new_root("root").append_frame("alpha", Some("hello"));
        let child = root.append_frame("beta", None);

        assert_eq!(child.current_depth(), 2);
        assert_eq!(child.lineage[0].skill_name, "alpha");
        assert_eq!(child.lineage[1].skill_name, "beta");
        assert_eq!(child.lineage[1].depth, 2);
        assert_eq!(
            child.lineage[1].parent_call_id,
            Some(child.lineage[0].call_id.clone())
        );
        assert!(child.lineage[0].args_digest.is_some());
    }

    #[test]
    fn test_effective_limits_shrink_against_parent_budget() {
        let budget = CallChainBudget {
            remaining_steps: Some(12),
            remaining_timeout_sec: Some(30),
        };

        let (steps, timeout) = effective_limits(&budget, Some(10), Some(60), 5, 60);
        assert_eq!(steps, 6);
        assert_eq!(timeout, 30);
    }

    #[test]
    fn test_effective_limits_without_parent_budget_use_defaults_without_shrink() {
        let budget = CallChainBudget::default();

        let (steps, timeout) = effective_limits(&budget, None, None, 5, 60);

        assert_eq!(steps, 5);
        assert_eq!(timeout, 60);
    }

    #[test]
    fn test_effective_limits_without_parent_budget_honor_explicit_request() {
        let budget = CallChainBudget::default();

        let (steps, timeout) = effective_limits(&budget, Some(8), Some(90), 5, 60);

        assert_eq!(steps, 8);
        assert_eq!(timeout, 90);
    }

    #[test]
    fn test_build_skill_activation_command_formats_targets() {
        assert_eq!(
            build_skill_activation_command("review", Some("src/lib.rs"), None),
            "/review src/lib.rs"
        );
        assert_eq!(
            build_skill_activation_command(
                "review",
                None,
                Some(&serde_json::json!({"path":"src/lib.rs"}))
            ),
            r#"/review {"path":"src/lib.rs"}"#
        );
        assert_eq!(
            build_skill_activation_command("review", None, None),
            "/review"
        );
    }
}
