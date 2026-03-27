//! Migration utility: convert legacy skill format to unified `SkillDef`.
//!
//! Legacy format (current `skills/*.md`):
//! ```text
//! ---
//! name: foo
//! description: Does foo
//! parameters:
//!   arg1:
//!     type: string
//!     description: First arg
//!     required: true
//! ---
//! echo "{{arg1}}"
//! ```

use super::definition::{
    SkillConstraints, SkillDef, SkillMeta, SkillPreamble, SkillTrigger,
};

/// Attempt to convert a legacy skill markdown string into a `SkillDef`.
///
/// If the content has a `parameters` field in its frontmatter, it is treated
/// as a legacy script-template skill. The script body becomes a preamble
/// (shell script) and the instructions explain how to invoke it.
pub fn migrate_legacy_skill(content: &str) -> Option<SkillDef> {
    let parts: Vec<&str> = content.splitn(3, "---").collect();
    if parts.len() < 3 {
        return None;
    }

    let yaml_str = parts[1].trim();
    let script_body = parts[2].trim().to_string();

    let yaml_val: serde_yaml::Value = serde_yaml::from_str(yaml_str).ok()?;

    let name = yaml_val.get("name")?.as_str()?.to_string();
    let description = yaml_val
        .get("description")?
        .as_str()
        .unwrap_or("")
        .to_string();

    // Check if this has parameters (legacy indicator)
    let has_params = yaml_val.get("parameters").is_some();

    if !has_params {
        // Not a legacy skill — try the unified parser instead
        return None;
    }

    // Build parameter documentation for instructions
    let mut param_docs = String::new();
    if let Some(params) = yaml_val.get("parameters").and_then(|p| p.as_mapping()) {
        param_docs.push_str("## Parameters\n\n");
        for (k, v) in params {
            let key = k.as_str().unwrap_or("?");
            let desc = v
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            let ptype = v.get("type").and_then(|t| t.as_str()).unwrap_or("string");
            param_docs.push_str(&format!("- `{}` ({}): {}\n", key, ptype, desc));
        }
        param_docs.push('\n');
    }

    let instructions = format!(
        "# {} (Legacy Migrated)\n\n{}\
         This skill runs the following script:\n\n```bash\n{}\n```\n",
        name, param_docs, script_body
    );

    let parameters_json: Option<serde_json::Value> = yaml_val
        .get("parameters")
        .and_then(|p| serde_json::to_value(p).ok());

    Some(SkillDef {
        meta: SkillMeta {
            name,
            version: "0.1.0-legacy".to_string(),
            description,
            trigger: SkillTrigger::ManualOnly,
            // Legacy skills typically need execute_bash
            allowed_tools: vec!["execute_bash".to_string()],
            output_mode: None,
            parameters: parameters_json.clone(),
        },
        instructions,
        preamble: Some(SkillPreamble {
            shell: script_body,
            tier: None,
        }),
        parameters: parameters_json,
        constraints: SkillConstraints::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_migrate_legacy_skill() {
        let md = r#"---
name: say_hello
description: Says hello to a person
parameters:
  person_name:
    type: string
    description: The name of the person
    required: true
---
echo "Hello {{person_name}}"
"#;
        let def = migrate_legacy_skill(md).unwrap();
        assert_eq!(def.meta.name, "say_hello");
        assert_eq!(def.meta.version, "0.1.0-legacy");
        assert!(def.instructions.contains("Legacy Migrated"));
        assert!(def.instructions.contains("person_name"));
        assert!(def.preamble.is_some());
        assert!(def
            .preamble
            .unwrap()
            .shell
            .contains("Hello {{person_name}}"));
    }

    #[test]
    fn test_non_legacy_returns_none() {
        // No parameters field → not legacy
        let md = r#"---
name: new_skill
description: A new skill
---
# Instructions
body
"#;
        assert!(migrate_legacy_skill(md).is_none());
    }

    #[test]
    fn test_migrate_generate_image() {
        let md = r#"---
name: generate_image
description: Generates an image from a text prompt using Google Gemini 2.5 Flash Image API.
parameters:
  prompt:
    type: string
    description: The text description of the image to generate.
    required: true
  output_path:
    type: string
    description: The local path where the resulting image will be saved.
    required: true
---

python3 skills/scripts/generate_image.py "{{prompt}}" "{{output_path}}"
"#;
        let def = migrate_legacy_skill(md).unwrap();
        assert_eq!(def.meta.name, "generate_image");
        assert!(def.instructions.contains("prompt"));
        assert!(def.instructions.contains("output_path"));
    }
}
