use super::model::FunctionResponse;

pub(crate) fn strip_thinking_tags(text: &str) -> String {
    let mut result = text.to_string();
    while let Some(start) = result.find("<think>") {
        if let Some(end_offset) = result[start..].find("</think>") {
            let end = start + end_offset;
            let before = &result[..start];
            let after = &result[end + 8..];
            result = format!("{}{}", before, after);
        } else {
            result = result[..start].to_string();
            break;
        }
    }
    result.trim().to_string()
}

pub(crate) fn strip_response_payload(fr: &mut FunctionResponse) {
    let obj = match fr.response.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    let result_val = match obj.get_mut("result") {
        Some(v) => v,
        None => return,
    };
    let result_str = match result_val.as_str() {
        Some(s) => s.to_string(),
        None => return,
    };

    let mut envelope: serde_json::Value = match serde_json::from_str(&result_str) {
        Ok(v) => v,
        Err(_) => {
            if result_str.len() > 500 {
                let head: String = result_str.chars().take(200).collect();
                *result_val = serde_json::Value::String(format!(
                    "{}\n... [stripped {} chars]",
                    head,
                    result_str.len()
                ));
            }
            return;
        }
    };

    let env_obj = match envelope.as_object_mut() {
        Some(o) => o,
        None => return,
    };

    match fr.name.as_str() {
        "task_plan" => {
            *result_val = serde_json::Value::String("[plan updated]".to_string());
            return;
        }
        "read_file" => {
            if let Some(output) = env_obj.get_mut("output") {
                if let Some(s) = output.as_str() {
                    let line_count = s.lines().count();
                    if line_count > 10 {
                        let head: String = s.lines().take(5).collect::<Vec<_>>().join("\n");
                        let tail: String = s
                            .lines()
                            .rev()
                            .take(5)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect::<Vec<_>>()
                            .join("\n");
                        *output = serde_json::Value::String(format!(
                            "{}\n... [stripped {} lines] ...\n{}",
                            head, line_count, tail
                        ));
                    }
                }
            }
        }
        "execute_bash" => {
            if let Some(output) = env_obj.get_mut("output") {
                if let Some(s) = output.as_str() {
                    let char_count = s.chars().count();
                    if char_count > 500 {
                        let head: String = s.chars().take(200).collect();
                        let tail: String = s.chars().skip(char_count - 200).collect();
                        *output = serde_json::Value::String(format!(
                            "{}\n... [stripped {} chars] ...\n{}",
                            head, char_count, tail
                        ));
                    }
                }
            }
        }
        "web_fetch" | "web_search_tavily" => {
            if let Some(output) = env_obj.get_mut("output") {
                if let Some(s) = output.as_str() {
                    *output = serde_json::Value::String(format!(
                        "[web content stripped - {} chars]",
                        s.len()
                    ));
                }
            }
        }
        "skill" | "use_skill" => {
            if let Some(output) = env_obj.get_mut("output") {
                *output = serde_json::Value::String("Skill loaded.".to_string());
            }
        }
        "write_file" | "patch_file" => {}
        _ => {
            if let Some(output) = env_obj.get_mut("output") {
                if let Some(s) = output.as_str() {
                    if s.len() > 500 {
                        let head: String = s.chars().take(200).collect();
                        let tail: String = s.chars().skip(s.chars().count() - 100).collect();
                        *output = serde_json::Value::String(format!(
                            "{}\n... [stripped {} chars] ...\n{}",
                            head,
                            s.len(),
                            tail
                        ));
                    }
                }
            }
        }
    }

    env_obj.remove("duration_ms");
    env_obj.remove("truncated");
    env_obj.remove("recovery_attempted");
    env_obj.remove("recovery_output");
    env_obj.remove("recovery_rule");

    if let Ok(stripped_str) = serde_json::to_string(env_obj) {
        *result_val = serde_json::Value::String(stripped_str);
    }
}
