//! Skill execution state — runtime data for an active skill session.

use std::collections::BTreeMap;

use super::definition::SkillConstraints;

/// The state of a currently active skill within a session.
#[derive(Debug, Clone)]
pub struct ActiveSkillState {
    pub skill_name: String,
    pub execution_state: SkillExecutionState,
    /// Arbitrary labels for skill-specific business phases.
    pub labels: BTreeMap<String, String>,
    /// Collected user answers keyed by context_key.
    pub answers: BTreeMap<String, SkillAnswer>,
    /// A pending question waiting for user input.
    pub pending_interaction: Option<PendingInteraction>,
    /// Artifacts produced during skill execution.
    pub artifacts: Vec<SkillArtifact>,
    /// Arguments provided at activation (e.g. from slash command).
    pub initial_args: Option<String>,
    /// Constraints inherited from the SkillDef.
    pub constraints: SkillConstraints,
}

/// Generic execution state — controlled by the runtime, not the skill.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillExecutionState {
    Running,
    WaitingUser,
    WaitingSubagent,
    ValidatingArtifacts,
    Completed,
}

/// A user's answer to a structured question.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SkillAnswer {
    pub question: String,
    pub answer: String,
    pub answered_at: String,
}

/// A pending structured question awaiting user response.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PendingInteraction {
    pub skill_name: String,
    pub context_key: String,
    pub question: String,
    pub options: Vec<String>,
    pub recommendation: Option<String>,
    pub asked_at: String,
}

/// An artifact produced by the skill.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SkillArtifact {
    pub kind: String,
    pub path: String,
    pub summary: Option<String>,
}

impl ActiveSkillState {
    /// Create a new state for the given skill.
    pub fn new(skill_name: String, constraints: SkillConstraints) -> Self {
        Self {
            skill_name,
            execution_state: SkillExecutionState::Running,
            labels: BTreeMap::new(),
            answers: BTreeMap::new(),
            pending_interaction: None,
            artifacts: Vec::new(),
            initial_args: None,
            constraints,
        }
    }

    /// Generate a concise summary for prompt injection.
    pub fn state_summary(&self) -> String {
        let mut parts = Vec::new();
        parts.push(format!("Skill: {}", self.skill_name));
        parts.push(format!("State: {:?}", self.execution_state));

        if !self.labels.is_empty() {
            let labels: Vec<String> = self
                .labels
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect();
            parts.push(format!("Labels: {}", labels.join(", ")));
        }

        if !self.answers.is_empty() {
            parts.push(format!("Answers collected: {}", self.answers.len()));
        }

        if let Some(pi) = &self.pending_interaction {
            parts.push(format!("PENDING QUESTION: {}", pi.question));
        }

        if !self.artifacts.is_empty() {
            parts.push(format!("Artifacts: {}", self.artifacts.len()));
        }

        if let Some(args) = &self.initial_args {
            parts.push(format!("USER INPUT AT ACTIVATION: {}", args));
        }

        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_state() {
        let state = ActiveSkillState::new("test".to_string(), SkillConstraints::default());
        assert_eq!(state.skill_name, "test");
        assert_eq!(state.execution_state, SkillExecutionState::Running);
        assert!(state.answers.is_empty());
    }

    #[test]
    fn test_state_summary() {
        let mut state = ActiveSkillState::new("review".to_string(), SkillConstraints::default());
        state.execution_state = SkillExecutionState::Running;
        state
            .labels
            .insert("phase".to_string(), "questioning".to_string());

        let summary = state.state_summary();
        assert!(summary.contains("review"));
        assert!(summary.contains("Running"));
        assert!(summary.contains("phase=questioning"));
    }
}
