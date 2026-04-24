use crate::context::{FileData, FunctionCall, Message};
use crate::tools::Tool;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tracing::Instrument;

use super::gemini_context;
use super::protocol::{
    create_standard_client, GeminiPlatform, LlmCapabilities, LlmClient, LlmError, StreamEvent,
};

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

#[derive(Clone)]
pub(crate) struct CachedContentInfo {
    pub(crate) id: String,
    pub(crate) hash: u64,
}

pub struct GeminiClient {
    api_key: String,
    client: Client,
    model_name: String,
    provider_name: String,
    platform: GeminiPlatform,
    #[allow(dead_code)]
    function_declarations_cache: Mutex<Option<CachedFunctionDeclarations>>,
    cached_content: Mutex<Option<CachedContentInfo>>,
    #[allow(dead_code)]
    context_window: usize,
}

struct CachedFunctionDeclarations {
    #[allow(dead_code)]
    signature: String,
    #[allow(dead_code)]
    declarations: Vec<FunctionDeclaration>,
}

impl GeminiClient {
    #[allow(dead_code)]
    pub fn new(api_key: String, model_name: Option<String>, provider_name: String) -> Self {
        let model_str = model_name
            .clone()
            .unwrap_or_else(|| "gemini-3.1-pro-preview".to_string());
        let base_url = "https://generativelanguage.googleapis.com";
        Self {
            api_key,
            client: create_standard_client(Some(base_url)),
            model_name: model_str,
            provider_name,
            platform: GeminiPlatform::Gen,
            function_declarations_cache: Mutex::new(None),
            cached_content: Mutex::new(None),
            context_window: 1_000_000,
        }
    }

    pub fn new_with_platform_and_window(
        api_key: String,
        model_name: Option<String>,
        #[allow(dead_code)] context_window: usize,
        provider_name: String,
        platform: GeminiPlatform,
    ) -> Self {
        let model_str = model_name
            .clone()
            .unwrap_or_else(|| "gemini-3.1-pro-preview".to_string());
        let base_url = match platform {
            GeminiPlatform::Gen => "https://generativelanguage.googleapis.com",
            GeminiPlatform::Vertex => "https://aiplatform.googleapis.com",
        };
        Self {
            api_key,
            client: create_standard_client(Some(base_url)),
            model_name: model_str,
            provider_name,
            platform,
            function_declarations_cache: Mutex::new(None),
            cached_content: Mutex::new(None),
            context_window,
        }
    }

    fn get_function_declarations(&self, tools: &[Arc<dyn Tool>]) -> Vec<FunctionDeclaration> {
        gemini_context::build_function_declarations(tools)
    }

    async fn dehydrate_messages(&self, messages: &mut Vec<Message>) -> Result<(), LlmError> {
        gemini_context::dehydrate_messages(&self.client, &self.api_key, messages).await
    }

    async fn dehydrate_message(&self, msg: &mut Message) -> Result<(), LlmError> {
        gemini_context::dehydrate_message(&self.client, &self.api_key, msg).await
    }
}

#[async_trait]
impl LlmClient for GeminiClient {
    fn model_name(&self) -> &str {
        &self.model_name
    }

    fn provider_name(&self) -> &str {
        &self.provider_name
    }

    fn context_window(&self) -> usize {
        self.context_window
    }

    fn capabilities(&self) -> LlmCapabilities {
        LlmCapabilities {
            function_tools: true,
            custom_tools: false,
            parallel_tool_calls: false,
            supports_code_mode: true,
        }
    }

    async fn stream(
        &self,
        messages: Vec<Message>,
        system_instruction: Option<Message>,
        tools: Vec<Arc<dyn Tool>>,
    ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
        let mut messages = messages;
        let mut system_instruction = system_instruction;
        self.dehydrate_messages(&mut messages).await?;
        if let Some(ref mut sys_msg) = system_instruction {
            self.dehydrate_message(sys_msg).await?;
        }

        let cached_content_id = gemini_context::resolve_cached_content(
            &self.client,
            &self.api_key,
            &self.model_name,
            &self.cached_content,
            &system_instruction,
            "system instruction",
        )
        .await;
        let final_system_instruction =
            gemini_context::final_system_instruction(&system_instruction, &cached_content_id);

        let function_declarations = self.get_function_declarations(&tools);
        let (tx, rx) = mpsc::channel(100);

        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let model_name = self.model_name.clone();
        let platform = self.platform;

        tokio::spawn(
            async move {
                let generation_config = gemini_context::text_generation_config(&model_name);

                let req_body = GeminiRequest {
                    contents: messages,
                    system_instruction: final_system_instruction,
                    tools: if function_declarations.is_empty() {
                        None
                    } else {
                        Some(vec![ToolDeclarationWrapper {
                            function_declarations,
                        }])
                    },
                    tool_config: None,
                    generation_config,
                    cached_content: cached_content_id,
                };

                let url = gemini_context::request_url(platform, &model_name, true);
                let body_json_string = gemini_context::request_body_json(platform, &req_body, None);

                let resp = match gemini_context::stream_connect_with_retry(
                    &client,
                    &api_key,
                    &url,
                    &body_json_string,
                )
                .await
                {
                    Ok(resp) => resp,
                    Err(message) => {
                        let _ = tx.send(StreamEvent::Error(message)).await;
                        return;
                    }
                };

                let mut stream = resp.bytes_stream();
                let mut buffer = String::new();
                let mut total_text_len: usize = 0;
                let mut total_tool_calls: usize = 0;
                let mut chunk_count: usize = 0;
                tracing::debug!("Gemini stream connected, starting to receive chunks");

                while let Some(chunk_res) = stream.next().await {
                    match chunk_res {
                        Ok(chunk) => {
                            let chunk_str = String::from_utf8_lossy(&chunk);
                            tracing::trace!("Received streaming chunk: {}", chunk_str);
                            buffer.push_str(&chunk_str);
                            while let Some(idx) =
                                buffer.find("\r\n\r\n").or_else(|| buffer.find("\n\n"))
                            {
                                let sep_len = if buffer.get(idx..idx + 4) == Some("\r\n\r\n") {
                                    4
                                } else {
                                    2
                                };
                                let line = buffer[..idx].trim().to_string();
                                buffer = buffer[idx + sep_len..].to_string();
                                if let Some(data) = line.strip_prefix("data: ") {
                                    if gemini_context::emit_sse_data_block(
                                        &tx,
                                        data,
                                        &mut total_text_len,
                                        &mut total_tool_calls,
                                        &mut chunk_count,
                                    )
                                    .await
                                    {
                                        return;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Gemini stream read error: {}", e);
                            let _ = tx
                                .send(StreamEvent::Error(format!("Stream read error: {}", e)))
                                .await;
                            return;
                        }
                    }
                }
                tracing::debug!(
                    "Gemini stream ended. chunks={}, total_text={} chars, tool_calls={}",
                    chunk_count,
                    total_text_len,
                    total_tool_calls
                );
                let _ = tx.send(StreamEvent::Done).await;
            }
            .in_current_span(),
        );

        Ok(rx)
    }
}

#[derive(Debug, Serialize)]
struct VertexFunctionCall {
    name: String,
    args: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

#[derive(Debug, Serialize)]
struct VertexFunctionResponse {
    name: String,
    response: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
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
                    id: fc.id.clone(),
                }),
                function_response: p
                    .function_response
                    .as_ref()
                    .map(|fr| VertexFunctionResponse {
                        name: fr.name.clone(),
                        response: fr.response.clone(),
                        id: fr.id.clone(),
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
                let def_name = ref_path
                    .strip_prefix(prefix1)
                    .or_else(|| ref_path.strip_prefix(prefix2));

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{FunctionCall, FunctionResponse, Part};
    use serde_json::json;

    #[test]
    fn test_to_vertex_message_thought_signature_isolation() {
        let model_msg = Message {
            role: "model".to_string(),
            parts: vec![Part {
                text: None,
                function_call: Some(FunctionCall {
                    name: "test_tool".to_string(),
                    args: json!({"arg": "value"}),
                    id: Some("call_1".to_string()),
                }),
                function_response: None,
                thought_signature: Some("sig_123".to_string()),
                file_data: None,
            }],
        };

        let vertex_model_msg = to_vertex_message(&model_msg);
        assert_eq!(
            vertex_model_msg.parts[0].thought_signature.as_deref(),
            Some("sig_123")
        );
        assert!(vertex_model_msg.parts[0].function_call.is_some());
        assert!(vertex_model_msg.parts[0].function_response.is_none());

        let function_msg = Message {
            role: "function".to_string(),
            parts: vec![Part {
                text: None,
                function_call: None,
                function_response: Some(FunctionResponse {
                    name: "test_tool".to_string(),
                    response: json!({"result": "success"}),
                    id: Some("call_1".to_string()),
                }),
                thought_signature: None,
                file_data: None,
            }],
        };

        let vertex_function_msg = to_vertex_message(&function_msg);
        assert_eq!(vertex_function_msg.parts[0].thought_signature, None);
        assert!(vertex_function_msg.parts[0].function_call.is_none());
        assert!(vertex_function_msg.parts[0].function_response.is_some());
    }
}
