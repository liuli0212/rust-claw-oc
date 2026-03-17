use crate::context::{FileData, FunctionCall, Message};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, Clone)]
pub struct GeminiRequest {
    pub contents: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "systemInstruction")]
    pub system_instruction: Option<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDeclarationWrapper>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "toolConfig")]
    pub tool_config: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "generationConfig")]
    pub generation_config: Option<GenerationConfig>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "cachedContent")]
    pub cached_content: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolDeclarationWrapper {
    #[serde(rename = "functionDeclarations")]
    pub function_declarations: Vec<FunctionDeclaration>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FunctionDeclaration {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "maxOutputTokens")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "thinkingConfig")]
    pub thinking_config: Option<ThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "responseMimeType")]
    pub response_mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "responseSchema")]
    pub response_schema: Option<Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ThinkingConfig {
    #[serde(rename = "includeThoughts")]
    pub include_thoughts: bool,
    #[serde(rename = "thinkingProcessQuotaTokens")]
    pub quota_tokens: u32,
}

#[derive(Debug, Serialize)]
struct VertexFunctionCall {
    name: String,
    args: Value,
}

#[derive(Debug, Serialize)]
struct VertexFunctionResponse {
    name: String,
    response: Value,
}

#[derive(Debug, Serialize)]
struct VertexPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "functionCall")]
    function_call: Option<VertexFunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "functionResponse")]
    function_response: Option<VertexFunctionResponse>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "thoughtSignature")]
    thought_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "fileData")]
    file_data: Option<FileData>,
}

#[derive(Debug, Serialize)]
pub(crate) struct VertexMessage {
    role: String,
    parts: Vec<VertexPart>,
}

#[derive(Debug, Serialize)]
pub(crate) struct VertexGeminiRequest {
    pub contents: Vec<VertexMessage>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "systemInstruction")]
    pub system_instruction: Option<VertexMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDeclarationWrapper>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "toolConfig")]
    pub tool_config: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "generationConfig")]
    pub generation_config: Option<GenerationConfig>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "cachedContent")]
    pub cached_content: Option<String>,
}

pub(crate) fn to_vertex_message(msg: &Message) -> VertexMessage {
    VertexMessage {
        role: msg.role.clone(),
        parts: msg
            .parts
            .iter()
            .map(|p| VertexPart {
                text: p.text.clone(),
                function_call: p.function_call.as_ref().map(|fc| VertexFunctionCall {
                    name: fc.name.clone(),
                    args: fc.args.clone(),
                }),
                function_response: p
                    .function_response
                    .as_ref()
                    .map(|fr| VertexFunctionResponse {
                        name: fr.name.clone(),
                        response: fr.response.clone(),
                    }),
                thought_signature: p.thought_signature.clone(),
                file_data: p.file_data.clone(),
            })
            .collect(),
    }
}

pub(crate) fn parse_function_call_basic(part: &Value) -> Option<FunctionCall> {
    let func_call = part.get("functionCall")?;
    let name = func_call.get("name")?.as_str()?.to_string();
    let args = func_call.get("args")?.clone();
    let id = func_call
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Some(FunctionCall { name, args, id })
}

pub(crate) fn capture_thought_signature(part: &Value) -> Option<String> {
    part.get("thoughtSignature")
        .or_else(|| part.get("thought_signature"))
        .and_then(|ts| ts.as_str())
        .map(|s| s.to_string())
}

pub(crate) fn inline_schema_refs(value: &mut Value, root: &Value, depth: usize) {
    if depth > 20 {
        return;
    }
    match value {
        Value::Object(map) => {
            if let Some(Value::String(ref_path)) = map.get("$ref") {
                let prefix1 = "#/$defs/";
                let prefix2 = "#/definitions/";
                let def_name = if ref_path.starts_with(prefix1) {
                    Some(&ref_path[prefix1.len()..])
                } else if ref_path.starts_with(prefix2) {
                    Some(&ref_path[prefix2.len()..])
                } else {
                    None
                };

                if let Some(name) = def_name {
                    let mut resolved = None;
                    if let Some(defs) = root.get("$defs").and_then(|v| v.as_object()) {
                        if let Some(def_val) = defs.get(name) {
                            resolved = Some(def_val.clone());
                        }
                    }
                    if resolved.is_none() {
                        if let Some(defs) = root.get("definitions").and_then(|v| v.as_object()) {
                            if let Some(def_val) = defs.get(name) {
                                resolved = Some(def_val.clone());
                            }
                        }
                    }

                    if let Some(mut resolved_val) = resolved {
                        inline_schema_refs(&mut resolved_val, root, depth + 1);
                        if let Value::Object(resolved_map) = resolved_val {
                            map.clear();
                            for (k, v) in resolved_map {
                                map.insert(k, v);
                            }
                        }
                    }
                }
            } else {
                for nested_val in map.values_mut() {
                    inline_schema_refs(nested_val, root, depth + 1);
                }
            }
        }
        Value::Array(arr) => {
            for nested_val in arr {
                inline_schema_refs(nested_val, root, depth + 1);
            }
        }
        _ => {}
    }
}

pub(crate) fn normalize_schema_for_gemini(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("$schema");
            map.remove("definitions");
            map.remove("$defs");
            map.remove("title");

            if let Some(type_val) = map.get_mut("type") {
                if let Value::Array(type_arr) = type_val {
                    let chosen = type_arr
                        .iter()
                        .filter_map(|v| v.as_str())
                        .find(|t| *t != "null")
                        .unwrap_or("string")
                        .to_string();
                    *type_val = Value::String(chosen);
                }
            }

            for combiner in ["anyOf", "oneOf", "allOf"] {
                if let Some(Value::Array(options)) = map.remove(combiner) {
                    let mut replacement = options
                        .into_iter()
                        .find(|candidate| candidate.get("$ref").is_none())
                        .unwrap_or(Value::Null);
                    normalize_schema_for_gemini(&mut replacement);
                    if let Value::Object(repl_map) = replacement {
                        for (k, v) in repl_map {
                            map.insert(k, v);
                        }
                    }
                }
            }

            if map.remove("$ref").is_some() {
                map.clear();
                map.insert("type".to_string(), Value::String("string".to_string()));
            }

            for nested in map.values_mut() {
                normalize_schema_for_gemini(nested);
            }
        }
        Value::Array(arr) => {
            for nested in arr {
                normalize_schema_for_gemini(nested);
            }
        }
        _ => {}
    }
}
