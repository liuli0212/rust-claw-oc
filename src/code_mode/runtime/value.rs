use serde_json::Value;

pub type StoredValue = Value;

pub fn normalize_tool_result_for_js(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => value.to_string(),
        Err(_) => Value::String(raw.to_string()).to_string(),
    }
}
