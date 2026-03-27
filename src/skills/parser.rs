//! Parser for unified `SKILL.md` format.
//!
//! Expected format:
//! ```text
//! ---
//! name: my_skill
//! version: "1.0"
//! description: Does something
//! ...
//! ---
//! # Instructions
//! Markdown body...
//! ```

use super::definition::{SkillConstraints, SkillDef, SkillMeta, SkillPreamble};
use serde::Deserialize;

/// Raw YAML frontmatter before conversion to `SkillDef`.
#[derive(Debug, Deserialize)]
struct RawFrontmatter {
    name: String,
    #[serde(default = "default_version")]
    version: String,
    description: String,
    #[serde(default)]
    trigger: Option<String>,
    #[serde(default)]
    allowed_tools: Option<Vec<String>>,
    #[serde(default)]
    output_mode: Option<String>,
    #[serde(default)]
    constraints: Option<SkillConstraints>,
    #[serde(default)]
    preamble: Option<RawPreamble>,
    // Legacy fields — script-template skills
    #[serde(default)]
    parameters: Option<serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
struct RawPreamble {
    shell: String,
    tier: Option<u8>,
}

fn default_version() -> String {
    "0.1.0".to_string()
}

/// Parse a unified `SKILL.md` file content into a `SkillDef`.
///
/// Returns `None` if the content does not have valid `---` delimited
/// YAML frontmatter.
pub fn parse_skill_md(content: &str) -> Option<SkillDef> {
    let parts: Vec<&str> = content.splitn(3, "---").collect();
    if parts.len() < 3 {
        return None;
    }

    let yaml_str = parts[1].trim();
    let body = parts[2].trim().to_string();

    let yaml_val: serde_yaml::Value = serde_yaml::from_str(yaml_str).ok()?;
    let raw: RawFrontmatter = serde_yaml::from_value(yaml_val.clone()).ok()?;
    let parameters_json: Option<serde_json::Value> = yaml_val
        .get("parameters")
        .and_then(|p| serde_json::to_value(p).ok());

    let trigger = match raw.trigger.as_deref() {
        Some("suggest_only") => super::definition::SkillTrigger::SuggestOnly,
        Some("manual_or_suggested") => super::definition::SkillTrigger::ManualOrSuggested,
        _ => super::definition::SkillTrigger::ManualOnly,
    };

    let output_mode = match raw.output_mode.as_deref() {
        Some("design_doc_only") => Some(super::definition::OutputMode::DesignDocOnly),
        Some("review_only") => Some(super::definition::OutputMode::ReviewOnly),
        Some("freeform") => Some(super::definition::OutputMode::Freeform),
        _ => None,
    };

    let preamble = raw.preamble.map(|p| SkillPreamble {
        shell: p.shell,
        tier: p.tier,
    });

    Some(SkillDef {
        meta: SkillMeta {
            name: raw.name,
            version: raw.version,
            description: raw.description,
            trigger,
            allowed_tools: raw.allowed_tools.unwrap_or_default(),
            output_mode,
            parameters: parameters_json,
        },
        instructions: body,
        preamble,
        parameters: raw
            .parameters
            .and_then(|p| serde_json::to_value(p).ok()),
        constraints: raw.constraints.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_skill() {
        let md = r#"---
name: hello
description: A simple greeting skill
---
# Instructions
Say hello to the user.
"#;
        let def = parse_skill_md(md).unwrap();
        assert_eq!(def.meta.name, "hello");
        assert_eq!(def.meta.description, "A simple greeting skill");
        assert_eq!(def.meta.version, "0.1.0");
        assert!(def.instructions.contains("Say hello"));
        assert!(def.preamble.is_none());
        assert!(!def.constraints.forbid_code_write);
    }

    #[test]
    fn test_parse_full_skill() {
        let md = r#"---
name: code_review
version: "2.0"
description: Reviews code changes
trigger: manual_or_suggested
allowed_tools: [read_file, execute_bash]
output_mode: review_only
constraints:
  forbid_code_write: true
  allow_subagents: true
  require_question_resume: true
  required_artifact_kind: review_report
preamble:
  shell: "echo READY=true"
  tier: 1
---
# Code Review Instructions
Review the code carefully.
"#;
        let def = parse_skill_md(md).unwrap();
        assert_eq!(def.meta.name, "code_review");
        assert_eq!(def.meta.version, "2.0");
        assert_eq!(
            def.meta.trigger,
            super::super::definition::SkillTrigger::ManualOrSuggested
        );
        assert_eq!(def.meta.allowed_tools, vec!["read_file", "execute_bash"]);
        assert!(def.constraints.forbid_code_write);
        assert!(def.constraints.allow_subagents);
        assert!(def.preamble.is_some());
        assert_eq!(def.preamble.unwrap().tier, Some(1));
    }

    #[test]
    fn test_parse_invalid_returns_none() {
        assert!(parse_skill_md("no frontmatter here").is_none());
        assert!(parse_skill_md("---\ninvalid: [yaml\n---\nbody").is_none());
    }
}
