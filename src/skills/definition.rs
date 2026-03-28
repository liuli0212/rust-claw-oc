//! Core data structures for the unified skill definition model.
//!
//! All skills — legacy script-template and new complex workflow — are
//! represented by `SkillDef`. Runtime behaviour (tool policy, preamble,
//! prompt injection) is driven by these structures.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Top-level unified skill definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDef {
    pub meta: SkillMeta,
    /// The skill's instruction body (markdown).
    pub instructions: String,
    /// Optional preamble to execute before skill activation.
    pub preamble: Option<SkillPreamble>,
    /// Optional parameter definitions (schema).
    pub parameters: Option<Value>,
    /// Runtime constraints and policies.
    pub constraints: SkillConstraints,
}

/// Skill metadata extracted from YAML frontmatter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMeta {
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    pub description: String,
    #[serde(default)]
    pub trigger: SkillTrigger,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub output_mode: Option<OutputMode>,
    /// Formal parameter definitions (as seen in YAML).
    pub parameters: Option<Value>,
}

/// How a skill can be triggered.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillTrigger {
    #[default]
    ManualOnly,
    SuggestOnly,
    ManualOrSuggested,
}

/// The kind of output a skill is limited to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    Freeform,
    DesignDocOnly,
    ReviewOnly,
}

/// Unified constraints — merges the original `SkillPolicies` + `SkillConstraints`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillConstraints {
    /// Hard gate: forbid writing code files.
    #[serde(default)]
    pub forbid_code_write: bool,
    /// Whether this skill may dispatch sub-agents.
    #[serde(default)]
    pub allow_subagents: bool,
    /// Whether user answers must go through structured question-resume.
    #[serde(default)]
    pub require_question_resume: bool,
    /// If set, the skill must produce an artifact of this kind before completion.
    #[serde(default)]
    pub required_artifact_kind: Option<ArtifactKind>,
}

/// The kind of artifact a skill is required to produce.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    DesignDoc,
    ReviewReport,
}

/// Preamble configuration — a shell script run once at skill activation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillPreamble {
    /// The shell command / script body.
    pub shell: String,
    /// Optional priority tier (lower = higher priority).
    pub tier: Option<u8>,
}

/// Whether this `SkillDef` originated from the new unified format or was
/// migrated from an old script-template.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub enum SkillOrigin {
    /// New unified `SKILL.md` format.
    #[default]
    Unified,
    /// Migrated from legacy `skills/*.md` script-template format.
    LegacyMigrated,
}

fn default_version() -> String {
    "0.1.0".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_constraints() {
        let c = SkillConstraints::default();
        assert!(!c.forbid_code_write);
        assert!(!c.allow_subagents);
        assert!(!c.require_question_resume);
        assert!(c.required_artifact_kind.is_none());
    }

    #[test]
    fn test_default_trigger() {
        let t = SkillTrigger::default();
        assert_eq!(t, SkillTrigger::ManualOnly);
    }
}
