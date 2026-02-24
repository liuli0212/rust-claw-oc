use crate::tools::{Tool, ToolError};
use async_trait::async_trait;
use serde_json::Value;
use std::process::Stdio;
use tokio::process::Command;

pub struct SkillTool {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub script_template: String,
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn parameters_schema(&self) -> Value {
        crate::tools::clean_schema(self.schema.clone())
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let mut script = self.script_template.clone();

        // Very basic template replacement for string arguments
        if let Some(obj) = args.as_object() {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    let placeholder = format!("{{{{{}}}}}", k);
                    script = script.replace(&placeholder, s);
                } else {
                    let placeholder = format!("{{{{{}}}}}", k);
                    script = script.replace(&placeholder, &v.to_string());
                }
            }
        }

        tracing::info!("Executing skill: {}", self.name);

        let output = Command::new("bash")
            .arg("-c")
            .arg(&script)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        let mut res = String::new();
        let stdout_str = String::from_utf8_lossy(&output.stdout);
        let stderr_str = String::from_utf8_lossy(&output.stderr);

        if !stdout_str.is_empty() {
            res.push_str("STDOUT:\n");
            res.push_str(&stdout_str);
        }
        if !stderr_str.is_empty() {
            if !res.is_empty() {
                res.push_str("\n");
            }
            res.push_str("STDERR:\n");
            res.push_str(&stderr_str);
        }

        if !output.status.success() {
            res.push_str(&format!(
                "\nExit code: {}",
                output.status.code().unwrap_or(-1)
            ));
        } else if res.is_empty() {
            res.push_str("Skill executed successfully with no output.");
        }

        Ok(res)
    }
}

pub fn load_skills(dir: &str) -> Vec<SkillTool> {
    let mut skills = Vec::new();
    let path = std::path::Path::new(dir);
    if !path.exists() || !path.is_dir() {
        return skills;
    }

    for entry in std::fs::read_dir(path).unwrap().flatten() {
        let file_path = entry.path();
        if file_path.is_file() && file_path.extension().map_or(false, |e| e == "md") {
            if let Ok(content) = std::fs::read_to_string(&file_path) {
                if let Some(skill) = parse_skill_markdown(&content) {
                    skills.push(skill);
                }
            }
        }
    }
    skills
}

fn parse_skill_markdown(content: &str) -> Option<SkillTool> {
    let parts: Vec<&str> = content.splitn(3, "---").collect();
    if parts.len() < 3 {
        return None;
    }

    let yaml_str = parts[1].trim();
    let script_template = parts[2].trim().to_string();

    let yaml_val: serde_yaml::Value = serde_yaml::from_str(yaml_str).ok()?;

    let name = yaml_val.get("name")?.as_str()?.to_string();
    let description = yaml_val.get("description")?.as_str()?.to_string();

    // Construct schema
    let mut schema = serde_json::json!({
        "type": "object",
        "properties": {},
        "required": []
    });

    if let Some(params) = yaml_val.get("parameters").and_then(|p| p.as_mapping()) {
        let mut required = Vec::new();
        let mut properties = serde_json::Map::new();

        for (k, v) in params {
            let k_str = k.as_str()?.to_string();
            let desc = v
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();
            let ptype = v
                .get("type")
                .and_then(|d| d.as_str())
                .unwrap_or("string")
                .to_string();

            let req = v.get("required").and_then(|r| r.as_bool()).unwrap_or(false);
            if req {
                required.push(k_str.clone());
            }

            properties.insert(
                k_str,
                serde_json::json!({
                    "type": ptype,
                    "description": desc
                }),
            );
        }

        schema["properties"] = serde_json::Value::Object(properties);
        schema["required"] = serde_json::Value::Array(
            required
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        );
    }

    Some(SkillTool {
        name,
        description,
        schema,
        script_template,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_skill() {
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
        let skill = parse_skill_markdown(md).unwrap();
        assert_eq!(skill.name, "say_hello");
        assert_eq!(skill.description, "Says hello to a person");
        assert!(skill
            .script_template
            .contains("echo \"Hello {{person_name}}\""));

        let schema_props = skill.schema.get("properties").unwrap();
        assert!(schema_props.get("person_name").is_some());
    }

    #[test]
    fn test_regression_parse_skill_markdown() {
        let md = r#"---
name: echo_number
description: Echoes a number
parameters:
  n:
    type: integer
    description: Number to print
    required: true
---
echo "{{n}}"
"#;

        let skill = parse_skill_markdown(md).unwrap();
        assert_eq!(skill.name, "echo_number");
        assert_eq!(skill.description, "Echoes a number");
        assert!(skill.script_template.contains("{{n}}"));
        assert_eq!(skill.schema["required"], serde_json::json!(["n"]));
    }
}
