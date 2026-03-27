use super::model::FunctionResponse;
use crate::tools::protocol::ToolExecutionEnvelope;

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

fn truncate_chars_with_marker(input: &str, head_chars: usize, tail_chars: usize) -> String {
    let char_count = input.chars().count();
    if char_count <= head_chars + tail_chars {
        return input.to_string();
    }

    let head: String = input.chars().take(head_chars).collect();
    let tail: String = input
        .chars()
        .skip(char_count.saturating_sub(tail_chars))
        .collect();
    format!(
        "{}\n... [stripped {} chars] ...\n{}",
        head, char_count, tail
    )
}

fn truncate_lines_with_marker(input: &str, head_lines: usize, tail_lines: usize) -> String {
    let lines: Vec<_> = input.lines().collect();
    if lines.len() <= head_lines + tail_lines {
        return input.to_string();
    }

    let head = lines
        .iter()
        .take(head_lines)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let tail = lines
        .iter()
        .rev()
        .take(tail_lines)
        .copied()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "{}\n... [stripped {} lines] ...\n{}",
        head,
        lines.len(),
        tail
    )
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

    let mut envelope: ToolExecutionEnvelope = match ToolExecutionEnvelope::from_json_str(&result_str) {
        Some(v) => v,
        None => {
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

    let tool_name = if envelope.result.tool_name.is_empty() {
        fr.name.as_str()
    } else {
        envelope.result.tool_name.as_str()
    };
    let evidence_kind = envelope.effects.evidence_kind.as_deref();
    let payload_kind = envelope.effects.payload_kind.as_deref();

    match (payload_kind, tool_name) {
        (Some("plan"), _) | (_, "task_plan") => {
            *result_val = serde_json::Value::String("[plan updated]".to_string());
            return;
        }
        _ if evidence_kind == Some("file") => {
            if envelope.result.output.lines().count() > 10 {
                envelope.result.output =
                    truncate_lines_with_marker(&envelope.result.output, 5, 5);
            }
        }
        _ if matches!(evidence_kind, Some("diagnostic" | "directory"))
            || tool_name == "execute_bash" =>
        {
            if envelope.result.output.chars().count() > 500 {
                envelope.result.output =
                    truncate_chars_with_marker(&envelope.result.output, 200, 200);
            }
        }
        (Some("web_content" | "web_search"), _) | (_, "web_fetch" | "web_search_tavily") => {
            envelope.result.output = format!(
                "[web content stripped - {} chars]",
                envelope.result.output.len()
            );
        }
        (Some("skill"), _) | (_, "skill" | "use_skill") => {
            envelope.result.output = "Skill loaded.".to_string();
        }
        (_, "write_file" | "patch_file") => {}
        _ => {
            if envelope.result.output.len() > 500 {
                envelope.result.output =
                    truncate_chars_with_marker(&envelope.result.output, 200, 100);
            }
        }
    }

    let mut envelope_value = match serde_json::to_value(&envelope) {
        Ok(value) => value,
        Err(_) => return,
    };
    let env_obj = match envelope_value.as_object_mut() {
        Some(obj) => obj,
        None => return,
    };
    env_obj.remove("duration_ms");
    env_obj.remove("truncated");
    env_obj.remove("recovery_attempted");
    env_obj.remove("recovery_output");
    env_obj.remove("recovery_rule");

    if let Ok(stripped_str) = serde_json::to_string(env_obj) {
        *result_val = serde_json::Value::String(stripped_str);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::model::FunctionResponse;

    #[test]
    fn strip_response_payload_uses_envelope_metadata() {
        let mut response = FunctionResponse {
            name: "finish_task".to_string(),
            id: None,
            response: serde_json::json!({
                "result": serde_json::json!({
                    "ok": true,
                    "tool_name": "read_file",
                    "output": "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11",
                    "evidence_kind": "file",
                    "finish_task_summary": "done"
                }).to_string()
            }),
        };

        strip_response_payload(&mut response);

        let result = response.response["result"].as_str().unwrap();
        assert!(result.contains("line1"));
        assert!(result.contains("stripped 11 lines"));
        assert!(result.contains("\"finish_task_summary\":\"done\""));
    }

    #[test]
    fn strip_response_payload_uses_payload_kind_for_plan_and_web() {
        let mut plan_response = FunctionResponse {
            name: "unknown_tool".to_string(),
            id: None,
            response: serde_json::json!({
                "result": serde_json::json!({
                    "ok": true,
                    "tool_name": "task_plan",
                    "payload_kind": "plan",
                    "output": "full plan contents"
                }).to_string()
            }),
        };

        strip_response_payload(&mut plan_response);
        assert_eq!(
            plan_response.response["result"].as_str(),
            Some("[plan updated]")
        );

        let mut web_response = FunctionResponse {
            name: "unknown_tool".to_string(),
            id: None,
            response: serde_json::json!({
                "result": serde_json::json!({
                    "ok": true,
                    "tool_name": "web_fetch",
                    "payload_kind": "web_content",
                    "output": "abcdefghijklmnopqrstuvwxyz"
                }).to_string()
            }),
        };

        strip_response_payload(&mut web_response);

        let result = web_response.response["result"].as_str().unwrap();
        assert!(result.contains("[web content stripped - 26 chars]"));
    }

    #[test]
    fn strip_response_payload_uses_payload_kind_for_skill() {
        let mut skill_response = FunctionResponse {
            name: "dynamic_tool".to_string(),
            id: None,
            response: serde_json::json!({
                "result": serde_json::json!({
                    "ok": true,
                    "tool_name": "echo_skill",
                    "payload_kind": "skill",
                    "output": "STDOUT:\\nhello"
                }).to_string()
            }),
        };

        strip_response_payload(&mut skill_response);

        let result = skill_response.response["result"].as_str().unwrap();
        assert!(result.contains("\"output\":\"Skill loaded.\""));
    }
}
