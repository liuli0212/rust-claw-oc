use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::skills::definition::SkillDef;
use crate::skills::policy::SkillToolPolicy;
use crate::skills::registry::SkillRegistry;
use crate::tools::protocol::ToolContext;

pub const MAX_DELEGATION_CALLS_PER_ROOT_REQUEST: usize = 6;
pub const MAX_DELEGATION_DEPTH: usize = 3;

#[derive(Debug, Clone)]
pub struct DelegationFrame {
    pub skill_name: String,
    pub call_id: String,
    pub parent_call_id: Option<String>,
    pub depth: usize,
    pub args_digest: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DelegationContext {
    pub lineage: Vec<DelegationFrame>,
    pub total_delegations: Arc<AtomicUsize>,
    pub root_session_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct DelegationBudget {
    pub remaining_steps: Option<usize>,
    pub remaining_timeout_sec: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct DelegationSessionSeed {
    pub inherited_context: Option<DelegationContext>,
    pub inherited_budget: DelegationBudget,
    pub delegated_context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DelegationFailureKind {
    MissingTools,
    BudgetExceeded,
    Timeout,
    CycleDetected,
    DepthExceeded,
    PolicyDenied,
    InteractiveSkill,
    ChildExecutionFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationFailure {
    pub kind: DelegationFailureKind,
    pub message: String,
    pub retryable: bool,
    pub llm_action_hint: Option<String>,
    pub details: Value,
}

#[derive(Debug, Clone)]
pub struct SkillDelegationRequest {
    pub skill_name: String,
    pub raw_args: Option<String>,
    pub json_args: Option<Value>,
    pub context: String,
    pub requested_timeout_sec: Option<u64>,
    pub requested_max_steps: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct ResolvedSkillDelegation {
    pub skill: SkillDef,
    pub activation_command: String,
    pub display_goal: String,
    pub lineage: Vec<String>,
    pub effective_tools: Vec<String>,
    pub effective_max_steps: usize,
    pub effective_timeout_sec: u64,
    pub allow_subagent_tool: bool,
    pub delegation_seed: DelegationSessionSeed,
}

impl DelegationContext {
    pub fn new_root(root_session_id: impl Into<String>) -> Self {
        Self {
            lineage: Vec::new(),
            total_delegations: Arc::new(AtomicUsize::new(0)),
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

    pub fn total_delegations_used(&self) -> usize {
        self.total_delegations.load(Ordering::SeqCst)
    }

    pub fn append_frame(&self, skill_name: &str, args: Option<&str>) -> Self {
        let mut lineage = self.lineage.clone();
        let depth = lineage.len() + 1;
        let parent_call_id = lineage.last().map(|frame| frame.call_id.clone());
        lineage.push(DelegationFrame {
            skill_name: skill_name.to_string(),
            call_id: format!("delegation_{}", uuid::Uuid::new_v4().simple()),
            parent_call_id,
            depth,
            args_digest: args.map(args_digest),
        });
        Self {
            lineage,
            total_delegations: self.total_delegations.clone(),
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
    budget: &DelegationBudget,
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

pub fn skill_is_interactive(skill: &SkillDef, policy: &SkillToolPolicy) -> bool {
    let allowed_tools = policy.canonicalize_tools(&skill.meta.allowed_tools);
    allowed_tools.iter().any(|tool| tool == "ask_user_question")
}

pub fn resolve_skill_delegation(
    registry: &SkillRegistry,
    policy: &SkillToolPolicy,
    tool_ctx: &ToolContext,
    runtime_available_tools: &[String],
    request: SkillDelegationRequest,
    default_max_steps: usize,
    default_timeout_sec: u64,
) -> Result<ResolvedSkillDelegation, DelegationFailure> {
    let skill_name = request.skill_name.trim();
    let serialized_args = request
        .json_args
        .as_ref()
        .map(Value::to_string)
        .or_else(|| request.raw_args.clone());
    let parent_context = tool_ctx
        .delegation_context
        .clone()
        .unwrap_or_else(|| DelegationContext::new_root(tool_ctx.session_id.clone()));
    let child_context_preview = parent_context.append_frame(skill_name, serialized_args.as_deref());
    let child_lineage = child_context_preview.lineage_names();

    if parent_context.contains_skill(skill_name) {
        return Err(DelegationFailure {
            kind: DelegationFailureKind::CycleDetected,
            message: format!(
                "Denied delegated skill run: cycle detected: {}",
                child_lineage.join(" -> ")
            ),
            retryable: false,
            llm_action_hint: Some(
                "Choose a different skill or finish the current skill without re-entering it."
                    .to_string(),
            ),
            details: json!({
                "skill_name": skill_name,
                "lineage": child_lineage,
            }),
        });
    }

    if child_context_preview.current_depth() > MAX_DELEGATION_DEPTH {
        return Err(DelegationFailure {
            kind: DelegationFailureKind::DepthExceeded,
            message: format!(
                "Denied delegated skill run: max delegation depth exceeded ({})",
                MAX_DELEGATION_DEPTH
            ),
            retryable: false,
            llm_action_hint: Some(
                "Flatten the plan or delegate to a sibling skill instead of nesting deeper."
                    .to_string(),
            ),
            details: json!({
                "skill_name": skill_name,
                "lineage": child_lineage,
                "max_depth": MAX_DELEGATION_DEPTH,
            }),
        });
    }

    if parent_context.total_delegations_used() >= MAX_DELEGATION_CALLS_PER_ROOT_REQUEST {
        return Err(DelegationFailure {
            kind: DelegationFailureKind::BudgetExceeded,
            message: format!(
                "Denied delegated skill run: max total delegations exceeded ({})",
                MAX_DELEGATION_CALLS_PER_ROOT_REQUEST
            ),
            retryable: false,
            llm_action_hint: Some(
                "Do not delegate again. Summarize the work already completed or finish directly."
                    .to_string(),
            ),
            details: json!({
                "skill_name": skill_name,
                "lineage": child_lineage,
                "used": parent_context.total_delegations_used(),
                "max_total_delegations": MAX_DELEGATION_CALLS_PER_ROOT_REQUEST,
            }),
        });
    }

    let Some(skill) = registry.clone_skill(skill_name) else {
        return Err(DelegationFailure {
            kind: DelegationFailureKind::PolicyDenied,
            message: format!("Denied delegated skill run: unknown skill '{skill_name}'"),
            retryable: false,
            llm_action_hint: Some(
                "Use one of the registered skills or continue without delegating.".to_string(),
            ),
            details: json!({
                "skill_name": skill_name,
                "available_skills": registry.names(),
            }),
        });
    };

    if skill.meta.allowed_tools.is_empty() {
        return Err(DelegationFailure {
            kind: DelegationFailureKind::PolicyDenied,
            message: format!(
                "Denied delegated skill run: skill '{}' must declare explicit allowed_tools before it can run in a subagent.",
                skill.meta.name
            ),
            retryable: false,
            llm_action_hint: Some(
                "Run this skill at the top level instead, or add an explicit allowed_tools whitelist."
                    .to_string(),
            ),
            details: json!({
                "skill_name": skill.meta.name,
                "reason": "missing_explicit_allowed_tools",
            }),
        });
    }

    if skill_is_interactive(&skill, policy) {
        return Err(DelegationFailure {
            kind: DelegationFailureKind::InteractiveSkill,
            message: format!(
                "Denied delegated skill run: skill '{}' is interactive and can only run at the top level.",
                skill.meta.name
            ),
            retryable: false,
            llm_action_hint: Some(
                "Ask the parent agent to run this skill directly in the main session.".to_string(),
            ),
            details: json!({
                "skill_name": skill.meta.name,
                "allowed_tools": skill.meta.allowed_tools,
            }),
        });
    }

    let callee_declared_tools = policy.canonicalize_tools(&skill.meta.allowed_tools);
    let runtime_available_tools = if tool_ctx.visible_tools.is_empty() {
        policy.canonicalize_tools(runtime_available_tools)
    } else {
        policy.canonicalize_tools(&tool_ctx.visible_tools)
    };
    let missing_tools: Vec<String> = callee_declared_tools
        .iter()
        .filter(|tool| !runtime_available_tools.contains(tool))
        .cloned()
        .collect();

    if !missing_tools.is_empty() {
        return Err(DelegationFailure {
            kind: DelegationFailureKind::MissingTools,
            message: format!(
                "Denied delegated skill run: child requires tools unavailable in the current runtime: [{}]",
                missing_tools.join(", ")
            ),
            retryable: false,
            llm_action_hint: Some(
                "Choose a different skill or continue without those unavailable tools.".to_string(),
            ),
            details: json!({
                "skill_name": skill.meta.name,
                "lineage": child_lineage,
                "missing_tools": missing_tools,
                "runtime_available_tools": runtime_available_tools,
            }),
        });
    }

    let effective_tools: Vec<String> = callee_declared_tools
        .iter()
        .filter(|tool| runtime_available_tools.contains(tool))
        .cloned()
        .collect();
    let (effective_max_steps, effective_timeout_sec) = effective_limits(
        &tool_ctx.delegation_budget,
        request.requested_max_steps,
        request.requested_timeout_sec,
        default_max_steps,
        default_timeout_sec,
    );
    let delegated_context = {
        let trimmed = request.context.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    };

    Ok(ResolvedSkillDelegation {
        skill,
        activation_command: build_skill_activation_command(
            skill_name,
            request.raw_args.as_deref(),
            request.json_args.as_ref(),
        ),
        display_goal: format!("delegated skill '{}'", skill_name),
        lineage: child_lineage,
        allow_subagent_tool: effective_tools.iter().any(|tool| tool == "subagent"),
        effective_tools,
        effective_max_steps,
        effective_timeout_sec,
        delegation_seed: DelegationSessionSeed {
            inherited_context: Some(parent_context),
            inherited_budget: DelegationBudget {
                remaining_steps: Some(effective_max_steps),
                remaining_timeout_sec: Some(effective_timeout_sec),
            },
            delegated_context,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::definition::{SkillConstraints, SkillMeta, SkillTrigger};
    use crate::skills::registry::SkillRegistry;
    use crate::tools::protocol::ToolContext;

    fn make_skill(name: &str, allowed_tools: &[&str]) -> SkillDef {
        SkillDef {
            meta: SkillMeta {
                name: name.to_string(),
                version: "1.0".to_string(),
                description: "test".to_string(),
                trigger: SkillTrigger::ManualOnly,
                allowed_tools: allowed_tools
                    .iter()
                    .map(|tool| (*tool).to_string())
                    .collect(),
                output_mode: None,
            },
            instructions: "test".to_string(),
            parameters: None,
            constraints: SkillConstraints::default(),
        }
    }

    fn make_ctx(
        lineage: &[&str],
        visible_tools: &[&str],
        remaining_steps: usize,
        remaining_timeout_sec: u64,
    ) -> ToolContext {
        let mut ctx = ToolContext::new("parent", "cli");
        ctx.visible_tools = visible_tools
            .iter()
            .map(|tool| (*tool).to_string())
            .collect();
        if !lineage.is_empty() {
            let mut call_context = DelegationContext::new_root("root");
            for skill in lineage {
                call_context = call_context.append_frame(skill, None);
            }
            ctx.delegation_context = Some(call_context);
        }
        ctx.delegation_budget = DelegationBudget {
            remaining_steps: Some(remaining_steps),
            remaining_timeout_sec: Some(remaining_timeout_sec),
        };
        ctx
    }

    #[test]
    fn test_append_frame_tracks_depth_and_parent() {
        let root = DelegationContext::new_root("root").append_frame("alpha", Some("hello"));
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
        let budget = DelegationBudget {
            remaining_steps: Some(12),
            remaining_timeout_sec: Some(30),
        };

        let (steps, timeout) = effective_limits(&budget, Some(10), Some(60), 5, 60);
        assert_eq!(steps, 6);
        assert_eq!(timeout, 30);
    }

    #[test]
    fn test_effective_limits_without_parent_budget_use_defaults_without_shrink() {
        let budget = DelegationBudget::default();

        let (steps, timeout) = effective_limits(&budget, None, None, 5, 60);

        assert_eq!(steps, 5);
        assert_eq!(timeout, 60);
    }

    #[test]
    fn test_effective_limits_without_parent_budget_honor_explicit_request() {
        let budget = DelegationBudget::default();

        let (steps, timeout) = effective_limits(&budget, Some(8), Some(90), 5, 60);

        assert_eq!(steps, 8);
        assert_eq!(timeout, 90);
    }

    #[test]
    fn test_skill_is_interactive_uses_allowed_tools() {
        let policy = SkillToolPolicy::new();
        let interactive = make_skill("interactive", &["ask_user_question"]);
        let non_interactive = make_skill("worker", &["read_file"]);

        assert!(skill_is_interactive(&interactive, &policy));
        assert!(!skill_is_interactive(&non_interactive, &policy));
    }

    #[test]
    fn test_resolve_skill_delegation_tracks_lineage_and_allows_nested_subagent() {
        let mut registry = SkillRegistry::new();
        registry.insert(make_skill("child_skill", &["read_file", "subagent"]));
        let policy = SkillToolPolicy::new();
        let ctx = make_ctx(&["planner"], &["read_file", "subagent"], 12, 20);

        let resolved = resolve_skill_delegation(
            &registry,
            &policy,
            &ctx,
            &["read_file".to_string(), "subagent".to_string()],
            SkillDelegationRequest {
                skill_name: "child_skill".to_string(),
                raw_args: None,
                json_args: Some(json!({"path":"src/lib.rs"})),
                context: "Focus on parser flow.".to_string(),
                requested_timeout_sec: Some(60),
                requested_max_steps: Some(10),
            },
            5,
            60,
        )
        .expect("delegation should resolve");

        assert_eq!(
            resolved.lineage,
            vec!["planner".to_string(), "child_skill".to_string()]
        );
        assert!(resolved.allow_subagent_tool);
        assert!(resolved.effective_tools.contains(&"subagent".to_string()));
        assert_eq!(
            resolved.delegation_seed.delegated_context.as_deref(),
            Some("Focus on parser flow.")
        );
    }

    #[test]
    fn test_resolve_skill_delegation_rejects_skill_without_explicit_allowed_tools() {
        let mut registry = SkillRegistry::new();
        registry.insert(make_skill("wide_open", &[]));
        let policy = SkillToolPolicy::new();
        let ctx = make_ctx(&[], &["read_file"], 8, 20);

        let error = resolve_skill_delegation(
            &registry,
            &policy,
            &ctx,
            &["read_file".to_string()],
            SkillDelegationRequest {
                skill_name: "wide_open".to_string(),
                raw_args: None,
                json_args: None,
                context: String::new(),
                requested_timeout_sec: None,
                requested_max_steps: None,
            },
            5,
            60,
        )
        .unwrap_err();

        assert!(matches!(error.kind, DelegationFailureKind::PolicyDenied));
        assert!(
            error
                .message
                .contains("must declare explicit allowed_tools")
        );
    }
}
