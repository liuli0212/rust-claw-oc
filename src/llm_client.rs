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

use crate::utils::{truncate_log, truncate_log_error};
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

    fn estimate_context_window(model: &str) -> usize {
        let m = model.to_lowercase();
        if m.contains("gemini-2") || m.contains("gemini-3") {
            1_000_000
        } else if m.contains("1.5-pro") || m.contains("1.5-flash") {
            1_000_000
        } else if m.contains("gpt-4o") || m.contains("gpt-4-turbo") || m.contains("o1") || m.contains("o3") {
            128_000
        } else if m.contains("claude-3-5") || m.contains("claude-3-opus") {
            200_000
        } else if m.contains("deepseek") {
            64_000
        } else if m.contains("qwen") {
            128_000
        } else {
            128_000
        }
    }

    pub fn create_llm_client(
        provider: &str,
        model: Option<String>,
        config: &crate::config::AppConfig,
    ) -> Result<Arc<dyn LlmClient>, String> {
        if let Some(prov_config) = config.get_provider(provider) {
            tracing::info!("Initializing provider '{}' from config", provider);
            match prov_config.type_name.as_str() {
                "openai_compat" | "aliyun" => {
                    let api_key = if let Some(env_var) = &prov_config.api_key_env {
                        std::env::var(env_var).or_else(|_| {
                            prov_config.api_key.clone().ok_or_else(|| format!("API key not found in env var '{}' or config", env_var))
                        })?
                    } else {
                        prov_config.api_key.clone().ok_or_else(|| "API key must be provided in config".to_string())?
                    };
                    
                    let base_url = prov_config.base_url.clone()
                        .ok_or_else(|| "base_url required for openai_compat".to_string())?;
                    let model_final = model
                        .or(prov_config.model.clone())
                        .unwrap_or_else(|| "gpt-3.5-turbo".to_string());

                    let context_window = prov_config.context_window
                        .unwrap_or_else(|| estimate_context_window(&model_final));

                    Ok(Arc::new(OpenAiCompatClient::new_with_window(api_key, base_url, model_final, provider.to_string(), context_window)))
                }
                "gemini" => {
                    let api_key = if let Some(env_var) = &prov_config.api_key_env {
                        std::env::var(env_var).or_else(|_| {
                            prov_config.api_key.clone().ok_or_else(|| format!("API key not found in env var '{}' or config", env_var))
                        })?
                    } else {
                        prov_config.api_key.clone().ok_or_else(|| "API key must be provided in config".to_string())?
                    };
                    let model_final = model.or(prov_config.model.clone());
                    let model_str = model_final.clone().unwrap_or_else(|| "gemini-3.1-pro-preview".to_string());
                    let context_window = prov_config.context_window
                        .unwrap_or_else(|| estimate_context_window(&model_str));

                    Ok(Arc::new(GeminiClient::new_with_window(api_key, model_final, context_window, provider.to_string())))
                }
                _ => Err(format!("Unknown provider type '{}'", prov_config.type_name)),
            }
        } else {
            // Fallback defaults if not in config
            match provider {
                "aliyun" => {
                    let api_key = std::env::var("DASHSCOPE_API_KEY")
                        .map_err(|_| "DASHSCOPE_API_KEY must be set for aliyun provider")?;
                    let model_final = model.unwrap_or_else(|| "qwen-plus".to_string());
                    tracing::info!("Using Aliyun provider with model: {}", model_final);
                    let context_window = estimate_context_window(&model_final);
                    Ok(Arc::new(OpenAiCompatClient::new_with_window(
                        api_key,
                        "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions".to_string(),
                        model_final,
                        "aliyun".to_string(),
                        context_window
                    )))
                }
                "gemini" | _ => {
                    let api_key = std::env::var("GEMINI_API_KEY")
                        .map_err(|_| "GEMINI_API_KEY must be set for gemini provider")?;
                    tracing::info!(
                        "Using Gemini provider with model: {:?}",
                        model.as_deref().unwrap_or("default")
                    );
                    let model_str = model.clone().unwrap_or_else(|| "gemini-3.1-pro-preview".to_string());
                    let context_window = estimate_context_window(&model_str);
                    Ok(Arc::new(GeminiClient::new_with_window(api_key, model, context_window, provider.to_string())))
                }
            }
        }
    }

#[async_trait]
pub trait LlmClient: Send + Sync {
    fn model_name(&self) -> &str;
    fn provider_name(&self) -> &str;
    fn context_window_size(&self) -> usize;
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
    provider_name: String,
    #[allow(dead_code)]
    function_declarations_cache: Mutex<Option<CachedFunctionDeclarations>>,
    context_window: usize,
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
    pub fn new(api_key: String, model_name: Option<String>, provider_name: String) -> Self {
        Self {
            api_key,
            client: Client::new(),
            model_name: model_name.unwrap_or_else(|| "gemini-3.1-pro-preview".to_string()),
            provider_name,
            function_declarations_cache: Mutex::new(None),
            context_window: 1_000_000,
        }
    }

    pub fn new_with_window(api_key: String, model_name: Option<String>, context_window: usize, provider_name: String) -> Self {
        Self {
            api_key,
            client: Client::new(),
            model_name: model_name.unwrap_or_else(|| "gemini-3.1-pro-preview".to_string()),
            provider_name,
            function_declarations_cache: Mutex::new(None),
            context_window,
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
    fn model_name(&self) -> &str {
        &self.model_name
    }
    fn provider_name(&self) -> &str {
        &self.provider_name
    }
    fn context_window_size(&self) -> usize {
        self.context_window
    }
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

        tracing::info!(
            "Gemini generate_text request: url={}, body={}",
            url,
            truncate_log(&serde_json::to_string(&req_body).unwrap_or_default())
        );

        let response = self
            .client
            .post(&url)
            .header(CONTENT_TYPE, "application/json")
            .json(&req_body)
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            tracing::error!("Gemini API Error: {}", truncate_log_error(&error_text));
            return Err(LlmError::ApiError(error_text));
        }

        let resp_json: Value = response.json().await?;
        let text = resp_json["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();
        tracing::info!("Gemini Response: {}", truncate_log(&text));
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

            let mut attempts = 0;
            let max_attempts = 5;
            let mut last_error = String::from("initialization");

            let resp = loop {
                attempts += 1;
                let body_json_string = serde_json::to_string(&req_body).unwrap_or_default();
                
                tracing::info!(
                    "Sending Gemini stream request (Attempt {}/{}): body={}",
                    attempts,
                    max_attempts,
                    truncate_log(&body_json_string)
                );

                let req_result = client
                    .post(&url)
                    .header(CONTENT_TYPE, "application/json")
                    .body(body_json_string)
                    .send()
                    .await;

                match req_result {
                    Ok(r) if r.status().is_success() => break r,
                    Ok(r) => {
                        let status = r.status();
                        let is_transient = status.is_server_error() || status.as_u16() == 429;
                        let body = r.text().await.unwrap_or_default();
                        last_error = format!("status={} body={}", status, truncate_log_error(&body));
                        
                        tracing::warn!(
                            "Gemini Stream API Error (Attempt {}/{}): {}",
                            attempts,
                            max_attempts,
                            last_error
                        );

                        if !is_transient || attempts >= max_attempts {
                            let _ = tx.send(StreamEvent::Error(format!(
                                "Gemini API error after {} attempts: {}",
                                attempts, last_error
                            ))).await;
                            return;
                        }
                    }
                    Err(e) => {
                        last_error = e.to_string();
                        tracing::warn!(
                            "Gemini Network Error (Attempt {}/{}): {}",
                            attempts,
                            max_attempts,
                            last_error
                        );

                        if attempts >= max_attempts {
                            let _ = tx.send(StreamEvent::Error(format!(
                                "Gemini network error after {} attempts: {}",
                                attempts, last_error
                            ))).await;
                            return;
                        }
                    }
                }

                let backoff = std::time::Duration::from_secs(1 << (attempts - 1));
                tracing::info!("Transient error detected. Retrying in {:?}...", backoff);
                tokio::time::sleep(backoff).await;
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
        id: None,
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
    provider_name: String,
    client: Client,
    context_window: usize,
}

impl OpenAiCompatClient {
    pub fn new(api_key: String, base_url: String, model_name: String, provider_name: String) -> Self {
        Self {
            api_key,
            base_url,
            model_name,
            provider_name,
            client: Client::new(),
            context_window: 32_000, // Default fallback
        }
    }

    pub fn new_with_window(api_key: String, base_url: String, model_name: String, provider_name: String, context_window: usize) -> Self {
        Self {
            api_key,
            base_url,
            model_name,
            provider_name,
            client: Client::new(),
            context_window,
        }
    }
}

#[async_trait]
impl LlmClient for OpenAiCompatClient {
    fn model_name(&self) -> &str {
        &self.model_name
    }
    fn provider_name(&self) -> &str {
        &self.provider_name
    }
    fn context_window_size(&self) -> usize {
        self.context_window
    }
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
            if msg.role == "user" {
                openai_messages.push(serde_json::json!({
                    "role": "user",
                    "content": msg.parts[0].text.as_deref().unwrap_or("")
                }));
            } else if msg.role == "model" {
                let text = msg.parts.iter().find_map(|p| p.text.as_deref()).unwrap_or("");
                let mut tool_calls = Vec::new();
                for part in &msg.parts {
                    if let Some(fc) = &part.function_call {
                        let call_id = fc.id.clone().unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4()));
                        tool_calls.push(serde_json::json!({
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": fc.name,
                                "arguments": fc.args.to_string()
                            }
                        }));
                    }
                }
                
                let mut message_json = serde_json::json!({
                    "role": "assistant",
                    "content": if text.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(text.to_string()) }
                });
                
                if !tool_calls.is_empty() {
                    message_json["tool_calls"] = serde_json::Value::Array(tool_calls);
                }
                openai_messages.push(message_json);
            } else if msg.role == "function" {
                 for part in &msg.parts {
                     if let Some(fr) = &part.function_response {
                         openai_messages.push(serde_json::json!({
                             "role": "tool",
                             "tool_call_id": fr.tool_call_id.clone().unwrap_or_else(|| "unknown".to_string()),
                             "content": fr.response.to_string()
                         }));
                     }
                 }
            }
        }

        let body = serde_json::json!({
            "model": self.model_name,
            "messages": openai_messages,
        });

        let body_json = serde_json::to_string(&body).unwrap_or_default();
        tracing::info!(
            "OpenAI generate_text request: url={}, body={}",
            self.base_url,
            truncate_log(&body_json)
        );
        let response = self
            .client
            .post(&self.base_url)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_else(|_| "Could not read error body".to_string());
            tracing::error!(
                "OpenAI API Error: status={}, url={}, body={}",
                status,
                self.base_url,
                truncate_log_error(&error_text)
            );
            return Err(LlmError::ApiError(format!("OpenAI API error: {} body={}", status, error_text)));
        }

        let resp_json: Value = response.json().await?;
        let text = resp_json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        tracing::info!("OpenAI Response: {}", truncate_log(&text));
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
            if msg.role == "user" {
                openai_messages.push(serde_json::json!({
                    "role": "user",
                    "content": msg.parts[0].text.as_deref().unwrap_or("")
                }));
            } else if msg.role == "model" {
                let text = msg.parts.iter().find_map(|p| p.text.as_deref()).unwrap_or("");
                let mut tool_calls = Vec::new();
                for part in &msg.parts {
                    if let Some(fc) = &part.function_call {
                        let call_id = fc.id.clone().unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4()));
                        tool_calls.push(serde_json::json!({
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": fc.name,
                                "arguments": fc.args.to_string()
                            }
                        }));
                    }
                }
                
                let mut message_json = serde_json::json!({
                    "role": "assistant",
                    "content": if text.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(text.to_string()) }
                });
                
                if !tool_calls.is_empty() {
                    message_json["tool_calls"] = serde_json::Value::Array(tool_calls);
                }
                openai_messages.push(message_json);
            } else if msg.role == "function" {
                 for part in &msg.parts {
                     if let Some(fr) = &part.function_response {
                         openai_messages.push(serde_json::json!({
                             "role": "tool",
                             "tool_call_id": fr.tool_call_id.clone().unwrap_or_else(|| "unknown".to_string()),
                             "content": fr.response.to_string()
                         }));
                     }
                 }
            }
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
            let mut attempts = 0;
            let max_attempts = 5;
            let mut last_error = String::from("initialization");

            let resp = loop {
                attempts += 1;
                let body_json_string = serde_json::to_string(&body_map).unwrap_or_default();
                
                tracing::info!(
                    "Sending stream request to {} (Attempt {}/{}): body={}",
                    base_url,
                    attempts,
                    max_attempts,
                    truncate_log(&body_json_string)
                );

                let req_result = client
                    .post(&base_url)
                    .header(AUTHORIZATION, format!("Bearer {}", api_key))
                    .header(CONTENT_TYPE, "application/json")
                    .body(body_json_string)
                    .send()
                    .await;

                match req_result {
                    Ok(r) if r.status().is_success() => break r,
                    Ok(r) => {
                        let status = r.status();
                        let is_transient = status.is_server_error() || status.as_u16() == 429;
                        let body = r.text().await.unwrap_or_default();
                        last_error = format!("status={} body={}", status, truncate_log_error(&body));
                        
                        tracing::warn!(
                            "OpenAI Stream API Error (Attempt {}/{}): {}",
                            attempts,
                            max_attempts,
                            last_error
                        );

                        if !is_transient || attempts >= max_attempts {
                            let _ = tx.send(StreamEvent::Error(format!(
                                "OpenAI API error after {} attempts: {}",
                                attempts, last_error
                            ))).await;
                            return;
                        }
                    }
                    Err(e) => {
                        last_error = e.to_string();
                        tracing::warn!(
                            "OpenAI Network Error (Attempt {}/{}): {}",
                            attempts,
                            max_attempts,
                            last_error
                        );

                        if attempts >= max_attempts {
                            let _ = tx.send(StreamEvent::Error(format!(
                                "OpenAI network error after {} attempts: {}",
                                attempts, last_error
                            ))).await;
                            return;
                        }
                    }
                }

                // Exponential backoff: 1s, 2s, 4s, 8s...
                let backoff = std::time::Duration::from_secs(1 << (attempts - 1));
                tracing::info!("Transient error detected. Retrying in {:?}...", backoff);
                tokio::time::sleep(backoff).await;
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
                            id: None,
                        }, None)).await;
                    }
                }
            }

            let _ = tx.send(StreamEvent::Done).await;
        });
        Ok(rx)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use crate::context::Part;

    #[tokio::test]
    #[ignore]
    async fn test_aliyun_qwen_generate() {
        let _ = dotenvy::dotenv();
        let api_key = env::var("DASHSCOPE_API_KEY");
        let api_key = env::var("DASHSCOPE_API_KEY");
        if api_key.is_err() {
            println!("Skipping test: DASHSCOPE_API_KEY not set");
            return;
        }
        let api_key = api_key.unwrap();
        
        let client = OpenAiCompatClient::new(
            api_key,
            "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions".to_string(),
            "qwen-plus".to_string(),
            "aliyun".to_string(),
        );

        let messages = vec![Message {
            role: "user".to_string(),
            parts: vec![Part {
                text: Some("Hello".to_string()),
                function_call: None,
                function_response: None,
                thought_signature: None,
            }],
        }];

        let result = client.generate_text(messages, None).await;
        match result {
            Ok(text) => println!("Aliyun Success: {}", text),
            Err(e) => panic!("Aliyun Failed: {}", e),
        }
    }
    #[test]
    fn test_estimate_context_window() {
        assert_eq!(estimate_context_window("gemini-1.5-pro"), 1_000_000);
        assert_eq!(estimate_context_window("gemini-1.5-flash"), 1_000_000);
        assert_eq!(estimate_context_window("gemini-2.0-flash"), 1_000_000);
        assert_eq!(estimate_context_window("gpt-4o"), 128_000);
        assert_eq!(estimate_context_window("gpt-4-turbo"), 128_000);
        assert_eq!(estimate_context_window("claude-3-5-sonnet"), 200_000);
        assert_eq!(estimate_context_window("deepseek-chat"), 64_000);
        assert_eq!(estimate_context_window("qwen-plus"), 128_000);
        assert_eq!(estimate_context_window("unknown-model"), 128_000);
    }
}

