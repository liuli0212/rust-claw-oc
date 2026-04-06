//! Runtime data for an active skill invocation.

use serde_json::Value;

use crate::delegation::DelegationContext;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillInvocationState {
    Running,
    WaitingUser,
}

#[derive(Debug, Clone)]
pub struct PendingInteraction {
    pub context_key: String,
    pub question: String,
    pub options: Vec<String>,
    pub recommendation: Option<String>,
    pub asked_at: String,
}

#[derive(Debug, Clone)]
pub struct SkillInvocation {
    pub skill_name: String,
    pub version: String,
    pub instructions: String,
    pub allowed_tools: Vec<String>,
    pub raw_args: Option<String>,
    pub json_args: Option<Value>,
    pub delegated_context: Option<String>,
    pub pending_interaction: Option<PendingInteraction>,
    pub state: SkillInvocationState,
    pub delegation_context: DelegationContext,
    pub compatibility_notes: Vec<String>,
}

impl SkillInvocation {
    pub fn state_summary(&self) -> String {
        let mut parts = Vec::new();
        parts.push(format!("Skill: {}", self.skill_name));
        parts.push(format!("State: {:?}", self.state));

        if !self.allowed_tools.is_empty() {
            parts.push(format!("Allowed tools: {}", self.allowed_tools.join(", ")));
        } else {
            parts.push("Allowed tools: all top-level tools".to_string());
        }

        if let Some(raw_args) = &self.raw_args {
            parts.push(format!("Activation args (raw): {}", raw_args));
        }

        if let Some(json_args) = &self.json_args {
            parts.push(format!("Activation args (json): {}", json_args));
        }

        if let Some(delegated_context) = &self.delegated_context {
            parts.push(format!("Delegation context: {}", delegated_context));
        }

        if let Some(pending) = &self.pending_interaction {
            parts.push(format!("Pending question: {}", pending.question));
        }

        if !self.compatibility_notes.is_empty() {
            parts.push(format!(
                "Compatibility notes: {}",
                self.compatibility_notes.join(" | ")
            ));
        }

        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_summary_includes_raw_and_json_args() {
        let invocation = SkillInvocation {
            skill_name: "review".to_string(),
            version: "1.0".to_string(),
            instructions: "test".to_string(),
            allowed_tools: vec!["read_file".to_string()],
            raw_args: Some("src/lib.rs".to_string()),
            json_args: Some(serde_json::json!({ "path": "src/lib.rs" })),
            delegated_context: Some("Please focus on parsing.".to_string()),
            pending_interaction: None,
            state: SkillInvocationState::Running,
            delegation_context: DelegationContext::new_root("root"),
            compatibility_notes: Vec::new(),
        };

        let summary = invocation.state_summary();
        assert!(summary.contains("Activation args (raw): src/lib.rs"));
        assert!(summary.contains("\"path\":\"src/lib.rs\""));
        assert!(summary.contains("Delegation context: Please focus on parsing."));
    }
}
