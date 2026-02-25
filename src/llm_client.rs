use crate::context::{FunctionCall, Message};
use crate::tools::Tool;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Error, Debug)]
pub enum LlmError {
    #[error("Network error: {0}")]
    NetworkError(#[from] reqwest::Error),
    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),
    #[error("API error: {0}")]
    ApiError(String),
}

#[derive(Debug)]
pub enum StreamEvent {
    Text(String),
    Thought(String),
    ToolCall(FunctionCall, Option<String>),
    Error(String),
    Done,
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn generate_text(
        &self,
        messages: Vec<Message>,
        system_instruction: Option<Message>,
    ) -> Result<String, LlmError>;

    async fn stream(
        &self,
        messages: Vec<Message>,
        system_instruction: Option<Message>,
        tools: Vec<Arc<dyn Tool>>,
    ) -> Result<mpsc::Receiver<StreamEvent>, LlmError>;
}

// --- Gemini Implementation ---

pub struct GeminiClient {
    api_key: String,
    client: Client,
    model_name: String,
    #[allow(dead_code)]
    function_declarations_cache: Mutex<Option<CachedFunctionDeclarations>>,
}

#[derive(Clone)]
struct CachedFunctionDeclarations {
    #[allow(dead_code)]
    signature: String,
    #[allow(dead_code)]
    declarations: Vec<FunctionDeclaration>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct GeminiRequest {
    pub contents: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "systemInstruction")]
    pub system_instruction: Option<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDeclarationWrapper>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "toolConfig")]
    pub tool_config: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ToolDeclarationWrapper {
    #[serde(rename = "functionDeclarations")]
    pub function_declarations: Vec<FunctionDeclaration>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct FunctionDeclaration {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl GeminiClient {
    pub fn new(api_key: String, model_name: Option<String>) -> Self {
        Self {
            api_key,
            client: Client::new(),
            model_name: model_name.unwrap_or_else(|| "gemini-3.1-pro-preview".to_string()),
            function_declarations_cache: Mutex::new(None),
        }
    }

    fn get_function_declarations(&self, tools: &[Arc<dyn Tool>]) -> Vec<FunctionDeclaration> {
        if tools.is_empty() {
            return Vec::new();
        }

        let mut declarations = Vec::with_capacity(tools.len());
        for tool in tools {
            let mut parameters = tool.parameters_schema();
            normalize_schema_for_gemini(&mut parameters);
            declarations.push(FunctionDeclaration {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters,
            });
        }
        declarations
    }
}

#[async_trait]
impl LlmClient for GeminiClient {
    async fn generate_text(
        &self,
        messages: Vec<Message>,
        system_instruction: Option<Message>,
    ) -> Result<String, LlmError> {
        let req_body = GeminiRequest {
            contents: messages,
            system_instruction,
            tools: None,
            tool_config: None,
        };

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.model_name, self.api_key
        );

        tracing::debug!(
            "Gemini generate_text request: url={}, body={}",
            url,
            serde_json::to_string(&req_body).unwrap_or_default()
        );

        let response = self
            .client
            .post(&url)
            .header(CONTENT_TYPE, "application/json")
            .json(&req_body)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(LlmError::ApiError(response.text().await?));
        }

        let resp_json: Value = response.json().await?;
        let text = resp_json["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();
        Ok(text)
    }

    async fn stream(
        &self,
        messages: Vec<Message>,
        system_instruction: Option<Message>,
        tools: Vec<Arc<dyn Tool>>,
    ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
        let function_declarations = self.get_function_declarations(&tools);
        let (tx, rx) = mpsc::channel(100);

        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let model_name = self.model_name.clone();

        tokio::spawn(async move {
            let req_body = GeminiRequest {
                contents: messages,
                system_instruction,
                tools: if function_declarations.is_empty() {
                    None
                } else {
                    Some(vec![ToolDeclarationWrapper {
                        function_declarations,
                    }])
                },
                tool_config: None,
            };

            let url = format!(
                "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?alt=sse&key={}",
                model_name, api_key
            );

            let body_json_string = serde_json::to_string(&req_body).unwrap_or_default();
            tracing::debug!("Gemini stream request: url={}, body={}", url, body_json_string);
            let resp = match client
                .post(&url)
                .header(CONTENT_TYPE, "application/json")
                .body(body_json_string)
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => {
                    let status = r.status();
                    let body = r.text().await.unwrap_or_default();
                    let _ = tx
                        .send(StreamEvent::Error(format!(
                            "Gemini API error: {} body={}",
                            status, body
                        )))
                        .await;
                    return;
                }
                Err(e) => {
                    let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                    return;
                }
            };

            let mut stream = resp.bytes_stream();
            let mut buffer = String::new();

            while let Some(chunk_res) = stream.next().await {
                if let Ok(chunk) = chunk_res {
                    let chunk_str = String::from_utf8_lossy(&chunk);
                    tracing::trace!("Received streaming chunk: {}", chunk_str);
                    buffer.push_str(&chunk_str);
                    while let Some(idx) = buffer.find("\r\n\r\n").or_else(|| buffer.find("\n\n")) {
                        let sep_len = if buffer.get(idx..idx + 4) == Some("\r\n\r\n") {
                            4
                        } else {
                            2
                        };
                        let line = buffer[..idx].trim().to_string();
                        buffer = buffer[idx + sep_len..].to_string();
                        if line.starts_with("data: ") {
                            let data = &line[6..];
                            if data == "[DONE]" {
                                let _ = tx.send(StreamEvent::Done).await;
                                return;
                            }
                            if let Ok(json) = serde_json::from_str::<Value>(data) {
                                if let Some(parts) =
                                    json["candidates"][0]["content"]["parts"].as_array()
                                {
                                    for part in parts {
                                        if let Some(thought) = part["thought"].as_str() {
                                            let _ = tx.send(StreamEvent::Thought(thought.to_string())).await;
                                        }
                                        if let Some(text) = part["text"].as_str() {
                                            let _ =
                                                tx.send(StreamEvent::Text(text.to_string())).await;
                                        }
                                        if let Some((func_call, signature)) = parse_function_call(part) {
                                            let _ = tx.send(StreamEvent::ToolCall(func_call, signature)).await;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            let _ = tx.send(StreamEvent::Done).await;
        });

        Ok(rx)
    }
}

fn parse_function_call(part: &Value) -> Option<(FunctionCall, Option<String>)> {
    let func_call = part.get("functionCall")?;
    let name = func_call.get("name")?.as_str()?.to_string();
    let args = func_call.get("args")?.clone();
    let thought_signature = part
        .get("thoughtSignature")
        .or_else(|| part.get("thought_signature"))
        .and_then(|ts| ts.as_str())
        .map(|s| s.to_string());
    Some((FunctionCall {
        name,
        args,
    }, thought_signature))
}

fn normalize_schema_for_gemini(value: &mut Value) {
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

// --- OpenAI Compatible Implementation (Aliyun DashScope) ---

pub struct OpenAiCompatClient {
    api_key: String,
    base_url: String,
    model_name: String,
    client: Client,
}

impl OpenAiCompatClient {
    pub fn new(api_key: String, base_url: String, model_name: String) -> Self {
        Self {
            api_key,
            base_url,
            model_name,
            client: Client::new(),
        }
    }
}

#[async_trait]
impl LlmClient for OpenAiCompatClient {
    async fn generate_text(
        &self,
        messages: Vec<Message>,
        system_instruction: Option<Message>,
    ) -> Result<String, LlmError> {
        let mut openai_messages = Vec::new();
        if let Some(sys) = system_instruction {
            openai_messages.push(serde_json::json!({
                "role": "system",
                "content": sys.parts[0].text.as_deref().unwrap_or("")
            }));
        }
        for msg in messages {
            let role = if msg.role == "user" {
                "user"
            } else {
                "assistant"
            };
            openai_messages.push(serde_json::json!({
                "role": role,
                "content": msg.parts[0].text.as_deref().unwrap_or("")
            }));
        }

        let body = serde_json::json!({
            "model": self.model_name,
            "messages": openai_messages,
        });

        let response = self
            .client
            .post(&self.base_url)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(LlmError::ApiError(response.text().await?));
        }

        let resp_json: Value = response.json().await?;
        let text = resp_json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        Ok(text)
    }

    async fn stream(
        &self,
        messages: Vec<Message>,
        system_instruction: Option<Message>,
        tools: Vec<Arc<dyn Tool>>,
    ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
        let (tx, rx) = mpsc::channel(100);

        let mut openai_messages = Vec::new();
        if let Some(sys) = system_instruction {
            openai_messages.push(serde_json::json!({
                "role": "system",
                "content": sys.parts[0].text.as_deref().unwrap_or("")
            }));
        }
        for msg in messages {
            let role = if msg.role == "user" {
                "user"
            } else {
                "assistant"
            };
            let content = msg
                .parts
                .iter()
                .find_map(|p| p.text.as_deref())
                .unwrap_or("");
            openai_messages.push(serde_json::json!({
                "role": role,
                "content": content
            }));
        }

        let mut body_map = serde_json::json!({
            "model": self.model_name,
            "messages": openai_messages,
            "stream": true,
            "parallel_tool_calls": false, // Prevent buggy multiple identical tool calls from Qwen/OpenAI
        });

        if !tools.is_empty() {
            let mut openai_tools = Vec::new();
            for tool in tools {
                openai_tools.push(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.name(),
                        "description": tool.description(),
                        "parameters": tool.parameters_schema(),
                    }
                }));
            }
            body_map["tools"] = serde_json::json!(openai_tools);
        }

        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let base_url = self.base_url.clone();

        tokio::spawn(async move {
            let body_json_string = serde_json::to_string(&body_map).unwrap_or_default();
            tracing::trace!("Sending request to OpenAI compat API: url={}, payload_size={} bytes", base_url, body_json_string.len());
            let resp = match client
                .post(&base_url)
                .header(AUTHORIZATION, format!("Bearer {}", api_key))
                .header(CONTENT_TYPE, "application/json")
                .body(body_json_string)
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => {
                    let _ = tx
                        .send(StreamEvent::Error(format!(
                            "OpenAI API error: {}",
                            r.status()
                        )))
                        .await;
                    return;
                }
                Err(e) => {
                    let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                    return;
                }
            };

            let mut stream = resp.bytes_stream();
            let mut buffer = String::new();

            // To properly parse OpenAI chunked tool calls (they can come with `index`)
            let mut active_tools: std::collections::HashMap<usize, (String, String)> = std::collections::HashMap::new();

            while let Some(chunk_res) = stream.next().await {
                if let Ok(chunk) = chunk_res {
                    buffer.push_str(&String::from_utf8_lossy(&chunk));

                    // Process complete lines
                    let mut lines = Vec::new();
                    while let Some(idx) = buffer.find('\n') {
                        lines.push(buffer[..idx].to_string());
                        buffer = buffer[idx + 1..].to_string();
                    }

                    for line in lines {
                        let line = line.trim();
                        if line.starts_with("data: ") {
                            let data = &line[6..];
                            if data == "[DONE]" {
                                continue;
                            }
                            if let Ok(json) = serde_json::from_str::<Value>(data) {
                                if let Some(choices) = json.get("choices").and_then(|v| v.as_array()) {
                                    for choice in choices {
                                        if let Some(delta) = choice.get("delta") {
                                            if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
                                                if !content.is_empty() {
                                                    let _ = tx.send(StreamEvent::Text(content.to_string())).await;
                                                }
                                            }
                                            if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                                                for tc in tool_calls {
                                                    let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                                                    let entry = active_tools.entry(idx).or_insert_with(|| (String::new(), String::new()));

                                                    if let Some(func) = tc.get("function") {
                                                        if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                                                            entry.0.push_str(name);
                                                        }
                                                        if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                                                            entry.1.push_str(args);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Flush remaining buffer if it looks like a line
            let final_line = buffer.trim();
            if final_line.starts_with("data: ") {
                let data = &final_line[6..];
                if data != "[DONE]" {
                    if let Ok(json) = serde_json::from_str::<Value>(data) {
                         if let Some(choices) = json.get("choices").and_then(|v| v.as_array()) {
                                    for choice in choices {
                                        if let Some(delta) = choice.get("delta") {
                                            if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
                                                if !content.is_empty() {
                                                    let _ = tx.send(StreamEvent::Text(content.to_string())).await;
                                                }
                                            }
                                            if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                                                for tc in tool_calls {
                                                    let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                                                    let entry = active_tools.entry(idx).or_insert_with(|| (String::new(), String::new()));

                                                    if let Some(func) = tc.get("function") {
                                                        if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                                                            entry.0.push_str(name);
                                                        }
                                                        if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                                                            entry.1.push_str(args);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                    }
                }
            }

            // Send all accumulated tool calls, sorted by index to preserve order
            let mut tool_indices: Vec<usize> = active_tools.keys().cloned().collect();
            tool_indices.sort_unstable();
            for idx in tool_indices {
                if let Some((name, args_str)) = active_tools.remove(&idx) {
                    if !name.trim().is_empty() {
                        let args = if args_str.trim().is_empty() {
                            serde_json::Value::Object(serde_json::Map::new())
                        } else {
                            serde_json::from_str(&args_str).unwrap_or(serde_json::Value::Null)
                        };
                        let _ = tx.send(StreamEvent::ToolCall(FunctionCall {
                            name,
                            args,
                        }, None)).await;
                    }
                }
            }

            let _ = tx.send(StreamEvent::Done).await;
        });

        Ok(rx)
    }
}
