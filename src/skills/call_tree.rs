use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub const MAX_DELEGATION_CALLS_PER_ROOT_REQUEST: usize = 6;

#[derive(Debug, Clone)]
pub struct SkillCallFrame {
    pub skill_name: String,
    pub call_id: String,
    pub parent_call_id: Option<String>,
    pub depth: usize,
    pub args_digest: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SkillCallContext {
    pub lineage: Vec<SkillCallFrame>,
    pub total_skill_calls: Arc<AtomicUsize>,
    pub root_session_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct SkillBudget {
    pub remaining_steps: Option<usize>,
    pub remaining_timeout_sec: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct SkillSessionSeed {
    pub inherited_call_context: Option<SkillCallContext>,
    pub inherited_budget: SkillBudget,
}

impl SkillCallContext {
    pub fn new_root(root_session_id: impl Into<String>) -> Self {
        Self {
            lineage: Vec::new(),
            total_skill_calls: Arc::new(AtomicUsize::new(0)),
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

    pub fn total_skill_calls_used(&self) -> usize {
        self.total_skill_calls.load(Ordering::SeqCst)
    }

    pub fn append_frame(&self, skill_name: &str, args: Option<&str>) -> Self {
        let mut lineage = self.lineage.clone();
        let depth = lineage.len() + 1;
        let parent_call_id = lineage.last().map(|frame| frame.call_id.clone());
        lineage.push(SkillCallFrame {
            skill_name: skill_name.to_string(),
            call_id: format!("skillcall_{}", uuid::Uuid::new_v4().simple()),
            parent_call_id,
            depth,
            args_digest: args.map(args_digest),
        });
        Self {
            lineage,
            total_skill_calls: self.total_skill_calls.clone(),
            root_session_id: self.root_session_id.clone(),
        }
    }
}

pub fn args_digest(input: &str) -> String {
    let mut hasher = DefaultHasher::new();
    input.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_append_frame_tracks_depth_and_parent() {
        let root = SkillCallContext::new_root("root").append_frame("alpha", Some("hello"));
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
    fn test_contains_skill_uses_lineage_names() {
        let ctx = SkillCallContext::new_root("root")
            .append_frame("alpha", None)
            .append_frame("beta", None);

        assert!(ctx.contains_skill("alpha"));
        assert!(ctx.contains_skill("beta"));
        assert!(!ctx.contains_skill("gamma"));
        assert_eq!(
            ctx.lineage_names(),
            vec!["alpha".to_string(), "beta".to_string()]
        );
    }
}
