use serde_json::{Map, Value};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SkillInvocationArgs {
    pub raw: Option<String>,
    pub json: Option<Value>,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum SkillArgumentError {
    #[error("Invalid JSON skill arguments: {0}")]
    InvalidJson(String),
    #[error("Skill argument validation failed: {0}")]
    Validation(String),
}

pub fn parse_invocation_args(
    raw_input: Option<&str>,
) -> Result<SkillInvocationArgs, SkillArgumentError> {
    let Some(raw_input) = raw_input.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(SkillInvocationArgs::default());
    };

    if raw_input.starts_with('{') {
        let json = serde_json::from_str(raw_input)
            .map_err(|error| SkillArgumentError::InvalidJson(error.to_string()))?;
        return Ok(SkillInvocationArgs {
            raw: None,
            json: Some(json),
        });
    }

    Ok(SkillInvocationArgs {
        raw: Some(raw_input.to_string()),
        json: None,
    })
}

pub fn validate_json_args(
    parameters: Option<&Value>,
    args: &Value,
) -> Result<(), SkillArgumentError> {
    let Some(parameters) = parameters else {
        return Ok(());
    };

    match normalize_parameter_spec(parameters)? {
        ParameterSpec::Object {
            properties,
            required,
            allow_additional,
        } => validate_object_args(args, &properties, &required, allow_additional),
    }
}

pub fn format_prompt_argument_sections(raw: Option<&str>, json: Option<&Value>) -> Option<String> {
    let mut sections = Vec::new();
    if let Some(json) = json {
        sections.push(format!("## Skill Arguments (JSON)\n{}", json));
    }
    if let Some(raw) = raw.filter(|value| !value.trim().is_empty()) {
        sections.push(format!("## Skill Arguments (Raw)\n{}", raw.trim()));
    }

    (!sections.is_empty()).then(|| sections.join("\n\n"))
}

enum ParameterSpec {
    Object {
        properties: Map<String, Value>,
        required: Vec<String>,
        allow_additional: bool,
    },
}

fn normalize_parameter_spec(parameters: &Value) -> Result<ParameterSpec, SkillArgumentError> {
    if let Some(object) = parameters.as_object() {
        let is_json_schema = object.contains_key("type") || object.contains_key("properties");
        if is_json_schema {
            let properties = object
                .get("properties")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let required = object
                .get("required")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let allow_additional = object
                .get("additionalProperties")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            return Ok(ParameterSpec::Object {
                properties,
                required,
                allow_additional,
            });
        }

        let mut properties = Map::new();
        let mut required = Vec::new();
        for (name, spec) in object {
            let Some(spec_object) = spec.as_object() else {
                return Err(SkillArgumentError::Validation(format!(
                    "parameter '{}' must be an object definition",
                    name
                )));
            };
            if spec_object
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                required.push(name.clone());
            }
            properties.insert(name.clone(), spec.clone());
        }
        return Ok(ParameterSpec::Object {
            properties,
            required,
            allow_additional: false,
        });
    }

    Err(SkillArgumentError::Validation(
        "parameters must be defined as an object".to_string(),
    ))
}

fn validate_object_args(
    args: &Value,
    properties: &Map<String, Value>,
    required: &[String],
    allow_additional: bool,
) -> Result<(), SkillArgumentError> {
    let Some(args_object) = args.as_object() else {
        return Err(SkillArgumentError::Validation(
            "structured skill arguments must be a JSON object".to_string(),
        ));
    };

    for required_key in required {
        if !args_object.contains_key(required_key) {
            return Err(SkillArgumentError::Validation(format!(
                "missing required parameter '{}'",
                required_key
            )));
        }
    }

    if !allow_additional {
        let unknown: Vec<String> = args_object
            .keys()
            .filter(|key| !properties.contains_key(*key))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            return Err(SkillArgumentError::Validation(format!(
                "unknown parameter(s): {}",
                unknown.join(", ")
            )));
        }
    }

    for (name, value) in args_object {
        let Some(spec) = properties.get(name).and_then(Value::as_object) else {
            continue;
        };
        if let Some(expected_type) = spec.get("type").and_then(Value::as_str) {
            if !value_matches_type(value, expected_type) {
                return Err(SkillArgumentError::Validation(format!(
                    "parameter '{}' must be of type {}",
                    name, expected_type
                )));
            }
        }
    }

    Ok(())
}

fn value_matches_type(value: &Value, expected_type: &str) -> bool {
    match expected_type {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_invocation_args_supports_raw_and_json() {
        let raw = parse_invocation_args(Some("src/lib.rs")).unwrap();
        assert_eq!(
            raw,
            SkillInvocationArgs {
                raw: Some("src/lib.rs".to_string()),
                json: None,
            }
        );

        let json_args = parse_invocation_args(Some(r#"{"path":"src/lib.rs"}"#)).unwrap();
        assert_eq!(
            json_args,
            SkillInvocationArgs {
                raw: None,
                json: Some(json!({"path":"src/lib.rs"})),
            }
        );
    }

    #[test]
    fn test_validate_json_args_for_map_style_parameters() {
        let parameters = json!({
            "path": { "type": "string", "required": true },
            "focus": { "type": "string", "required": false }
        });

        validate_json_args(Some(&parameters), &json!({"path":"src/lib.rs"})).unwrap();
        let error = validate_json_args(Some(&parameters), &json!({"focus":"bugs"})).unwrap_err();
        assert!(error
            .to_string()
            .contains("missing required parameter 'path'"));
    }

    #[test]
    fn test_validate_json_args_rejects_unknown_and_wrong_type() {
        let parameters = json!({
            "type": "object",
            "properties": {
                "count": { "type": "integer" }
            },
            "required": ["count"]
        });

        let error = validate_json_args(Some(&parameters), &json!({"count":"oops"})).unwrap_err();
        assert!(error
            .to_string()
            .contains("parameter 'count' must be of type integer"));
    }
}
