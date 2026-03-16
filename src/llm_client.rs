#![allow(warnings)]
use crate::context::{FunctionCall, Message};
use crate::tools::Tool;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::sync::Mutex;

use crate::utils::{format_full_error, truncate_log, truncate_log_error};
#[derive(Error, Debug)]
pub enum LlmError {
    #[error("Network error: {0}")]
    NetworkError(#[from] reqwest::Error),
    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),
    #[error("API error: {0}")]
    #[allow(dead_code)]
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
    } else if m.contains("gpt-4o")
        || m.contains("gpt-4-turbo")
        || m.contains("o1")
        || m.contains("o3")
    {
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
    platform_override: Option<String>,
    config: &crate::config::AppConfig,
) -> Result<Arc<dyn LlmClient>, String> {
    if let Some(prov_config) = config.get_provider(provider) {
        tracing::info!("Initializing provider '{}' from config", provider);
        match prov_config.type_name.as_str() {
            "openai_compat" | "aliyun" => {
                let raw_api_key = if let Some(env_var) = &prov_config.api_key_env {
                    std::env::var(env_var).or_else(|_| {
                        prov_config.api_key.clone().ok_or_else(|| {
                            format!("API key not found in env var '{}' or config", env_var)
                        })
                    })?
                } else {
                    prov_config
                        .api_key
                        .clone()
                        .ok_or_else(|| "API key must be provided in config".to_string())?
                };
                let api_key = raw_api_key.trim().to_string();

                let base_url = prov_config
                    .base_url
                    .clone()
                    .ok_or_else(|| "base_url required for openai_compat".to_string())?;
                let model_final = model
                    .or(prov_config.model.clone())
                    .unwrap_or_else(|| "gpt-3.5-turbo".to_string());

                let context_window = prov_config
                    .context_window
                    .unwrap_or_else(|| estimate_context_window(&model_final));

                Ok(Arc::new(OpenAiCompatClient::new_with_window(
                    api_key,
                    base_url,
                    model_final,
                    provider.to_string(),
                    context_window,
                    prov_config.reasoning_effort.clone(),
                )))
            }
            "gemini" => {
                let raw_api_key = if let Some(env_var) = &prov_config.api_key_env {
                    std::env::var(env_var).or_else(|_| {
                        prov_config.api_key.clone().ok_or_else(|| {
                            format!("API key not found in env var '{}' or config", env_var)
                        })
                    })?
                } else {
                    prov_config
                        .api_key
                        .clone()
                        .ok_or_else(|| "API key must be provided in config".to_string())?
                };
                let api_key = raw_api_key.trim().to_string();
                let model_final = model.or(prov_config.model.clone());
                let model_str = model_final
                    .clone()
                    .unwrap_or_else(|| "gemini-3.1-pro-preview".to_string());
                let context_window = prov_config
                    .context_window
                    .unwrap_or_else(|| estimate_context_window(&model_str));

                let platform = match platform_override.as_deref() {
                    Some("vertex") => GeminiPlatform::Vertex,
                    Some("gen") => GeminiPlatform::Gen,
                    _ => match prov_config.platform.as_deref() {
                        Some("gen") => GeminiPlatform::Gen,
                        _ => GeminiPlatform::Vertex,
                    },
                };

                Ok(Arc::new(GeminiClient::new_with_platform_and_window(
                    api_key,
                    model_final,
                    context_window,
                    provider.to_string(),
                    platform,
                )))
            }
            _ => Err(format!("Unknown provider type '{}'", prov_config.type_name)),
        }
    } else {
        // Fallback defaults if not in config
        match provider {
            "aliyun" => {
                let api_key = std::env::var("DASHSCOPE_API_KEY")
                    .map(|s| s.trim().to_string())
                    .map_err(|_| "DASHSCOPE_API_KEY must be set for aliyun provider")?;
                let model_final = model.unwrap_or_else(|| "qwen-plus".to_string());
                tracing::info!("Using Aliyun provider with model: {}", model_final);
                let context_window = estimate_context_window(&model_final);
                Ok(Arc::new(OpenAiCompatClient::new_with_window(
                    api_key,
                    "https://coding.dashscope.aliyuncs.com/v1/chat/completions".to_string(),
                    model_final,
                    "aliyun".to_string(),
                    context_window,
                    None,
                )))
            }
            "gemini" | _ => {
                let api_key = std::env::var("GEMINI_API_KEY")
                    .map(|s| s.trim().to_string())
                    .map_err(|_| "GEMINI_API_KEY must be set for gemini provider")?;
                tracing::info!(
                    "Using Gemini provider with model: {:?}",
                    model.as_deref().unwrap_or("default")
                );
                let model_str = model
                    .clone()
                    .unwrap_or_else(|| "gemini-3.1-pro-preview".to_string());
                let context_window = estimate_context_window(&model_str);
                let platform = match platform_override.as_deref() {
                    Some("gen") => GeminiPlatform::Gen,
                    _ => GeminiPlatform::Vertex,
                };
                Ok(Arc::new(GeminiClient::new_with_platform_and_window(
                    api_key,
                    model,
                    context_window,
                    provider.to_string(),
                    platform,
                )))
            }
        }
    }
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    fn model_name(&self) -> &str;
    fn provider_name(&self) -> &str;
    #[allow(dead_code)]
    fn context_window_size(&self) -> usize;
    #[allow(dead_code)]
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

    async fn generate_structured(
        &self,
        messages: Vec<Message>,
        system_instruction: Option<Message>,
        response_schema: Value,
    ) -> Result<Value, LlmError>;
}

// --- Gemini Implementation ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiPlatform {
    Gen,    // generativelanguage.googleapis.com
    Vertex, // aiplatform.googleapis.com
}

pub struct GeminiClient {
    api_key: String,
    client: Client,
    model_name: String,
    provider_name: String,
    platform: GeminiPlatform,
    #[allow(dead_code)]
    function_declarations_cache: Mutex<Option<CachedFunctionDeclarations>>,
    cached_content: tokio::sync::Mutex<Option<CachedContentInfo>>,
    #[allow(dead_code)]
    context_window: usize,
}

#[derive(Clone)]
struct CachedContentInfo {
    id: String,
    hash: u64,
}

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
    #[serde(skip_serializing_if = "Option::is_none", rename = "generationConfig")]
    pub generation_config: Option<GenerationConfig>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "cachedContent")]
    pub cached_content: Option<String>,
}

// --- Vertex-compatible types (no 'id' field) ---

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
    pub thought_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "fileData")]
    pub file_data: Option<crate::context::FileData>,
}

#[derive(Debug, Serialize)]
struct VertexMessage {
    role: String,
    parts: Vec<VertexPart>,
}

#[derive(Debug, Serialize)]
struct VertexGeminiRequest {
    contents: Vec<VertexMessage>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "systemInstruction")]
    system_instruction: Option<VertexMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolDeclarationWrapper>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "toolConfig")]
    pub tool_config: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "generationConfig")]
    pub generation_config: Option<GenerationConfig>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "cachedContent")]
    pub cached_content: Option<String>,
}

fn to_vertex_message(msg: &Message) -> VertexMessage {
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

fn create_standard_client(base_url: Option<&str>) -> Client {
    let mut builder = Client::builder()
        .connect_timeout(std::time::Duration::from_secs(300))
        .timeout(std::time::Duration::from_secs(600)) // 10 minutes for large context
        .pool_idle_timeout(std::time::Duration::from_secs(600)) // 10 minutes
        .pool_max_idle_per_host(10)
        .tcp_keepalive(Some(std::time::Duration::from_secs(30))) // More aggressive keepalive
        .http2_keep_alive_interval(Some(std::time::Duration::from_secs(15)))
        .http2_keep_alive_timeout(std::time::Duration::from_secs(20))
        .http2_keep_alive_while_idle(true)
        .http2_initial_stream_window_size(4 * 1024 * 1024)
        .http2_initial_connection_window_size(4 * 1024 * 1024)
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                "X-Server-Timeout",
                reqwest::header::HeaderValue::from_static("600"),
            );
            headers.insert(
                "x-goog-api-client",
                reqwest::header::HeaderValue::from_static("rusty-claw/0.1.0"),
            );
            headers
        })
        .gzip(true);

    // Explicitly check for NO_PROXY because reqwest might not pick it up correctly
    // from std::env if it was set by dotenvy after the process start.
    if let Some(url) = base_url {
        let no_proxy = std::env::var("no_proxy")
            .or_else(|_| std::env::var("NO_PROXY"))
            .unwrap_or_default();

        // Simple matching logic: if any entry in no_proxy matches the host or is a suffix
        let bypass = no_proxy.split(',').any(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return false;
            }
            if entry == "*" {
                return true;
            }

            // Check if URL contains the entry as a host or suffix (e.g., .srv)
            url.contains(entry)
        });

        if bypass {
            tracing::debug!("Bypassing proxy for URL: {} (matched in NO_PROXY)", url);
            builder = builder.no_proxy();
        }
    }

    builder.build().unwrap_or_else(|_| Client::new())
}

impl GeminiClient {
    #[allow(dead_code)]
    pub fn new(api_key: String, model_name: Option<String>, provider_name: String) -> Self {
        let model_str = model_name
            .clone()
            .unwrap_or_else(|| "gemini-3.1-pro-preview".to_string());
        // Gemini base URL is always the Google API
        let base_url = "https://generativelanguage.googleapis.com";
        Self {
            api_key,
            client: create_standard_client(Some(base_url)),
            model_name: model_str,
            provider_name,
            platform: GeminiPlatform::Gen,
            function_declarations_cache: Mutex::new(None),
            cached_content: tokio::sync::Mutex::new(None),
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
            cached_content: tokio::sync::Mutex::new(None),
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
            let root_schema = parameters.clone();
            inline_schema_refs(&mut parameters, &root_schema, 0);
            normalize_schema_for_gemini(&mut parameters);
            declarations.push(FunctionDeclaration {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters,
            });
        }
        declarations
    }

    /// Uploads content as a file to Gemini File API and returns the file URI.
    async fn upload_content(&self, content: &str, mime_type: &str) -> Result<String, LlmError> {
        let url = format!(
            "https://generativelanguage.googleapis.com/upload/v1beta/files?key={}",
            self.api_key
        );

        let metadata = serde_json::json!({
            "file": {
                "display_name": format!("payload_{}", uuid::Uuid::new_v4().simple()),
            }
        });

        tracing::info!(
            "Starting resumable upload to Gemini File API ({} bytes)",
            content.len()
        );

        let response = self
            .client
            .post(&url)
            .header("X-Goog-Upload-Protocol", "resumable")
            .header("X-Goog-Upload-Command", "start")
            .header(
                "X-Goog-Upload-Header-Content-Length",
                content.len().to_string(),
            )
            .header("X-Goog-Upload-Header-Content-Type", mime_type)
            .json(&metadata)
            .send()
            .await?;

        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            return Err(LlmError::ApiError(format!(
                "Failed to start upload: {}",
                error
            )));
        }

        let session_url = response
            .headers()
            .get("X-Goog-Upload-URL")
            .and_then(|v| v.to_str().ok())
            .unwrap();

        // 2. Multi-part chunked upload for robustness
        let bytes = content.as_bytes();
        let chunk_size = 5 * 1024 * 1024; // 5MB chunks
        let total_len = bytes.len();
        let mut offset = 0;
        let mut final_uri = String::new();

        while offset < total_len {
            let end = (offset + chunk_size).min(total_len);
            let chunk = bytes[offset..end].to_vec();
            let is_last = end == total_len;

            let upload_response = self
                .client
                .put(session_url)
                .header("X-Goog-Upload-Offset", offset.to_string())
                .header(
                    "X-Goog-Upload-Command",
                    if is_last {
                        "upload, finalize"
                    } else {
                        "upload"
                    },
                )
                .body(chunk)
                .send()
                .await?;

            if !upload_response.status().is_success() {
                let error = upload_response.text().await.unwrap_or_default();
                return Err(LlmError::ApiError(format!(
                    "Failed to upload chunk at offset {}: {}",
                    offset, error
                )));
            }

            if is_last {
                let final_json: serde_json::Value = upload_response.json().await?;
                final_uri = final_json["file"]["uri"].as_str().unwrap().to_string();
                break;
            }

            offset = end;
        }
        Ok(final_uri)
    }

    async fn dehydrate_messages(&self, messages: &mut Vec<Message>) -> Result<(), LlmError> {
        for msg in messages {
            self.dehydrate_message(msg).await?;
        }
        Ok(())
    }

    async fn dehydrate_message(&self, msg: &mut Message) -> Result<(), LlmError> {
        for part in &mut msg.parts {
            if let Some(text) = &part.text {
                if text.len() > 512 * 1024 {
                    let file_uri = self.upload_content(text, "text/plain").await?;
                    part.text = None;
                    part.file_data = Some(crate::context::FileData {
                        mime_type: "text/plain".to_string(),
                        file_uri,
                    });
                }
            }
        }
        Ok(())
    }

    async fn create_context_cache(&self, system_instruction: &Message) -> Result<String, LlmError> {
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/cachedContents?key={}",
            self.api_key
        );

        let body = serde_json::json!({
            "model": format!("models/{}", self.model_name),
            "systemInstruction": system_instruction,
            "ttl": "3600s"
        });

        let response = self.client.post(&url).json(&body).send().await?;
        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            return Err(LlmError::ApiError(format!(
                "Cache creation failed: {}",
                error
            )));
        }
        let json: serde_json::Value = response.json().await?;
        let name = json["name"]
            .as_str()
            .ok_or_else(|| LlmError::ApiError("No name in cache response".to_string()))?;
        Ok(name.to_string())
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
        let mut messages = messages;
        let mut system_instruction = system_instruction;
        self.dehydrate_messages(&mut messages).await?;
        if let Some(ref mut sys_msg) = system_instruction {
            self.dehydrate_message(sys_msg).await?;
        }

        let mut cached_content_id = None;
        if let Some(ref sys_msg) = system_instruction {
            // Check if caching is worth it (> 128KB roughly 32k tokens)
            let sys_str = serde_json::to_string(sys_msg).unwrap_or_default();
            if sys_str.len() > 128 * 1024 {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                sys_str.hash(&mut hasher);
                let current_hash = hasher.finish();

                let mut cache_guard = self.cached_content.lock().await;
                if let Some(cache_info) = &*cache_guard {
                    if cache_info.hash == current_hash {
                        cached_content_id = Some(cache_info.id.clone());
                    }
                }

                if cached_content_id.is_none() {
                    tracing::info!(
                        "Creating context cache for system instruction ({} bytes)",
                        sys_str.len()
                    );
                    match self.create_context_cache(sys_msg).await {
                        Ok(id) => {
                            *cache_guard = Some(CachedContentInfo {
                                id: id.clone(),
                                hash: current_hash,
                            });
                            cached_content_id = Some(id);
                        }
                        Err(e) => {
                            tracing::warn!("Failed to create context cache: {}", e);
                        }
                    }
                }
            }
        }

        // If we have a cache, we might want to clear system_instruction from the request
        // but only if it's identical to what's in the cache.
        // For simplicity, if cached_content_id is Some, we set system_instruction = None.
        let final_system_instruction = if cached_content_id.is_some() {
            None
        } else {
            system_instruction.clone()
        };

        let generation_config = if self.model_name.contains("thinking") {
            Some(GenerationConfig {
                temperature: Some(0.7),
                max_output_tokens: Some(64000),
                thinking_config: Some(ThinkingConfig {
                    include_thoughts: true,
                    quota_tokens: 32000,
                }),
                response_mime_type: None,
                response_schema: None,
            })
        } else {
            Some(GenerationConfig {
                temperature: Some(0.0),
                max_output_tokens: Some(8192),
                thinking_config: None,
                response_mime_type: None,
                response_schema: None,
            })
        };

        let req_body = GeminiRequest {
            contents: messages,
            system_instruction: final_system_instruction,
            tools: None,
            tool_config: None,
            generation_config: generation_config.clone(),
            cached_content: cached_content_id,
        };

        let req_body_json = serde_json::to_string(&req_body).unwrap_or_default();
        let url = match self.platform {
            GeminiPlatform::Gen => format!(
                "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent", self.model_name
            ),
            GeminiPlatform::Vertex => format!(
                "https://aiplatform.googleapis.com/v1beta1/publishers/google/models/{}:generateContent", self.model_name
            ),
        };

        tracing::info!(
            "Gemini generate_text request: url={}, body_size={} bytes",
            url,
            req_body_json.len()
        );
        tracing::debug!(
            "Gemini generate_text body: {}",
            truncate_log(&req_body_json)
        );

        let response = match self.platform {
            GeminiPlatform::Gen => {
                self.client
                    .post(&url)
                    .header(CONTENT_TYPE, "application/json")
                    .header("x-goog-api-key", self.api_key.clone())
                    .json(&req_body)
                    .send()
                    .await?
            }
            GeminiPlatform::Vertex => {
                let vertex_req = VertexGeminiRequest {
                    contents: req_body.contents.iter().map(to_vertex_message).collect(),
                    system_instruction: req_body.system_instruction.as_ref().map(to_vertex_message),
                    tools: req_body.tools.clone(),
                    tool_config: req_body.tool_config.clone(),
                    generation_config: req_body.generation_config.clone(),
                    cached_content: None,
                };
                self.client
                    .post(&url)
                    .header(CONTENT_TYPE, "application/json")
                    .header("x-goog-api-key", self.api_key.clone())
                    .json(&vertex_req)
                    .send()
                    .await?
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Could not read error body".to_string());
            let truncated_error = truncate_log_error(&error_text);
            tracing::error!(
                "Gemini API Error: status={}, body={}",
                status,
                truncated_error
            );
            return Err(LlmError::ApiError(format!(
                "Gemini API status={}: {}",
                status, truncated_error
            )));
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
        let mut messages = messages;
        let mut system_instruction = system_instruction;
        self.dehydrate_messages(&mut messages).await?;
        if let Some(ref mut sys_msg) = system_instruction {
            self.dehydrate_message(sys_msg).await?;
        }

        let mut cached_content_id = None;
        if let Some(ref sys_msg) = system_instruction {
            // Check if caching is worth it (> 128KB roughly 32k tokens)
            let sys_str = serde_json::to_string(sys_msg).unwrap_or_default();
            if sys_str.len() > 128 * 1024 {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                sys_str.hash(&mut hasher);
                let current_hash = hasher.finish();

                let mut cache_guard = self.cached_content.lock().await;
                if let Some(cache_info) = &*cache_guard {
                    if cache_info.hash == current_hash {
                        cached_content_id = Some(cache_info.id.clone());
                    }
                }

                if cached_content_id.is_none() {
                    tracing::info!(
                        "Creating context cache for system instruction ({} bytes)",
                        sys_str.len()
                    );
                    match self.create_context_cache(sys_msg).await {
                        Ok(id) => {
                            *cache_guard = Some(CachedContentInfo {
                                id: id.clone(),
                                hash: current_hash,
                            });
                            cached_content_id = Some(id);
                        }
                        Err(e) => {
                            tracing::warn!("Failed to create context cache: {}", e);
                        }
                    }
                }
            }
        }

        // If we have a cache, we might want to clear system_instruction from the request
        // but only if it's identical to what's in the cache.
        // For simplicity, if cached_content_id is Some, we set system_instruction = None.
        let final_system_instruction = if cached_content_id.is_some() {
            None
        } else {
            system_instruction.clone()
        };

        let function_declarations = self.get_function_declarations(&tools);
        let (tx, rx) = mpsc::channel(100);

        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let model_name = self.model_name.clone();
        let platform = self.platform;

        tokio::spawn(async move {
            let generation_config = if model_name.contains("thinking") {
                Some(GenerationConfig {
                    temperature: Some(0.7),
                    max_output_tokens: Some(64000),
                    thinking_config: Some(ThinkingConfig {
                        include_thoughts: true,
                        quota_tokens: 32000,
                    }),
                    response_mime_type: None,
                    response_schema: None,
                })
            } else {
                Some(GenerationConfig {
                    temperature: Some(0.0),
                    max_output_tokens: Some(8192),
                    thinking_config: None,
                    response_mime_type: None,
                    response_schema: None,
                })
            };

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

            let url = match platform {
                GeminiPlatform::Gen => format!(
                    "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?alt=sse", model_name
                ),
                GeminiPlatform::Vertex => format!(
                    "https://aiplatform.googleapis.com/v1beta1/publishers/google/models/{}:streamGenerateContent?alt=sse", model_name
                ),
            };

            let mut attempts = 0;
            let max_attempts = 5;
            let mut last_error = String::from("initialization");

            let body_json_string = match platform {
                GeminiPlatform::Gen => serde_json::to_string(&req_body).unwrap_or_default(),
                GeminiPlatform::Vertex => {
                    let vertex_req = VertexGeminiRequest {
                        contents: req_body.contents.iter().map(to_vertex_message).collect(),
                        system_instruction: req_body
                            .system_instruction
                            .as_ref()
                            .map(to_vertex_message),
                        tools: req_body.tools.clone(),
                        tool_config: req_body.tool_config.clone(),
                        generation_config: req_body.generation_config.clone(),
                        cached_content: None,
                    };
                    serde_json::to_string(&vertex_req).unwrap_or_default()
                }
            };

            let resp = loop {
                attempts += 1;

                tracing::info!(
                    "Sending Gemini stream request (Attempt {}/{}, body_size={} bytes)",
                    attempts,
                    max_attempts,
                    body_json_string.len()
                );
                tracing::debug!("Gemini stream body: {}", truncate_log(&body_json_string));

                let req_result = client
                    .post(&url)
                    .header(CONTENT_TYPE, "application/json")
                    .header("x-goog-api-key", api_key.clone())
                    .body(body_json_string.clone())
                    .send()
                    .await;

                match req_result {
                    Ok(r) if r.status().is_success() => break r,
                    Ok(r) => {
                        let status = r.status();
                        let is_transient = status.is_server_error() || status.as_u16() == 429;
                        let body = r.text().await.unwrap_or_default();
                        last_error =
                            format!("status={} body={}", status, truncate_log_error(&body));

                        tracing::warn!(
                            "Gemini Stream API Error (Attempt {}/{}): {}",
                            attempts,
                            max_attempts,
                            last_error
                        );

                        if !is_transient || attempts >= max_attempts {
                            let _ = tx
                                .send(StreamEvent::Error(format!(
                                    "Gemini API error after {} attempts: {}",
                                    attempts, last_error
                                )))
                                .await;
                            return;
                        }
                    }
                    Err(e) => {
                        last_error = format_full_error(&e);
                        tracing::warn!(
                            "Gemini Network Error (Attempt {}/{}):\n{}",
                            attempts,
                            max_attempts,
                            last_error
                        );

                        if attempts >= max_attempts {
                            let _ = tx
                                .send(StreamEvent::Error(format!(
                                    "Gemini network error after {} attempts: {}",
                                    attempts, last_error
                                )))
                                .await;
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
            let mut total_text_len: usize = 0;
            let mut total_tool_calls: usize = 0;
            let mut chunk_count: usize = 0;
            tracing::debug!("Gemini stream connected, starting to receive chunks");

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
                                tracing::debug!("Gemini stream received [DONE] signal");
                                let _ = tx.send(StreamEvent::Done).await;
                                return;
                            }
                            match serde_json::from_str::<Value>(data) {
                                Ok(json) => {
                                    chunk_count += 1;
                                    tracing::debug!(
                                        "Gemini SSE chunk #{}: keys={:?}\nRaw: {}",
                                        chunk_count,
                                        json.as_object().map(|m| m.keys().collect::<Vec<_>>()),
                                        crate::utils::truncate_log(data)
                                    );

                                    // 1. Check for thinking at candidate level (some models/versions)
                                    if let Some(candidate) =
                                        json["candidates"].as_array().and_then(|a| a.first())
                                    {
                                        if let Some(thought) =
                                            candidate.get("thought").and_then(|v| v.as_str())
                                        {
                                            if !thought.is_empty() {
                                                let _ = tx
                                                    .send(StreamEvent::Thought(thought.to_string()))
                                                    .await;
                                            }
                                        }
                                        // Also check for 'thinking' field
                                        if let Some(thinking) =
                                            candidate.get("thinking").and_then(|v| v.as_str())
                                        {
                                            if !thinking.is_empty() {
                                                let _ = tx
                                                    .send(StreamEvent::Thought(
                                                        thinking.to_string(),
                                                    ))
                                                    .await;
                                            }
                                        }
                                    }

                                    if let Some(parts) =
                                        json["candidates"][0]["content"]["parts"].as_array()
                                    {
                                        // Capture thought_signature from candidate level if not found in part
                                        let candidate_signature = json["candidates"][0]
                                            .get("thoughtSignature")
                                            .or_else(|| {
                                                json["candidates"][0].get("thought_signature")
                                            })
                                            .and_then(|ts| ts.as_str())
                                            .map(|s| s.to_string());

                                        for part in parts {
                                            tracing::trace!("Gemini part: {}", part);
                                            // Check if this is a thought part (Gemini sends thought: true boolean)
                                            let is_thought =
                                                part["thought"].as_bool().unwrap_or(false);

                                            // [Fix] Also check if 'thought' is a string itself (common in some API versions)
                                            if let Some(thought_text) = part["thought"].as_str() {
                                                let _ = tx
                                                    .send(StreamEvent::Thought(
                                                        thought_text.to_string(),
                                                    ))
                                                    .await;
                                            }

                                            if is_thought {
                                                // thought: true means the text field contains thinking content
                                                if let Some(text) = part["text"].as_str() {
                                                    let _ = tx
                                                        .send(StreamEvent::Thought(
                                                            text.to_string(),
                                                        ))
                                                        .await;
                                                    tracing::debug!(
                                                        "Gemini thought: {} chars, content={}",
                                                        text.len(),
                                                        crate::utils::truncate_log(text)
                                                    );
                                                }
                                            } else if let Some(text) = part["text"].as_str() {
                                                total_text_len += text.len();
                                                tracing::debug!(
                                                    "Gemini text: {} chars (total: {}), content={}",
                                                    text.len(),
                                                    total_text_len,
                                                    crate::utils::truncate_log(text)
                                                );
                                                let _ = tx
                                                    .send(StreamEvent::Text(text.to_string()))
                                                    .await;
                                            }
                                            if let Some(func_call) = parse_function_call_basic(part)
                                            {
                                                total_tool_calls += 1;
                                                tracing::debug!(
                                                    "Gemini tool_call: name={}",
                                                    func_call.name
                                                );
                                                let signature = capture_thought_signature(part)
                                                    .or_else(|| candidate_signature.clone());
                                                let _ = tx
                                                    .send(StreamEvent::ToolCall(
                                                        func_call, signature,
                                                    ))
                                                    .await;
                                            }
                                        }
                                    } else {
                                        tracing::debug!(
                                            "Gemini SSE chunk #{} has no candidates/parts. Raw: {}",
                                            chunk_count,
                                            truncate_log(data)
                                        );
                                    }
                                }
                                Err(parse_err) => {
                                    tracing::warn!(
                                        "Gemini SSE JSON parse error: {}. Raw data: {}",
                                        parse_err,
                                        truncate_log(data)
                                    );
                                }
                            }
                        }
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
        });

        Ok(rx)
    }

    async fn generate_structured(
        &self,
        messages: Vec<Message>,
        system_instruction: Option<Message>,
        response_schema: Value,
    ) -> Result<Value, LlmError> {
        let mut messages = messages;
        let mut system_instruction = system_instruction;
        self.dehydrate_messages(&mut messages).await?;
        if let Some(ref mut sys_msg) = system_instruction {
            self.dehydrate_message(sys_msg).await?;
        }

        let mut cached_content_id = None;
        if let Some(ref sys_msg) = system_instruction {
            let sys_str = serde_json::to_string(sys_msg).unwrap_or_default();
            if sys_str.len() > 128 * 1024 {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                sys_str.hash(&mut hasher);
                let current_hash = hasher.finish();

                let mut cache_guard = self.cached_content.lock().await;
                if let Some(cache_info) = &*cache_guard {
                    if cache_info.hash == current_hash {
                        cached_content_id = Some(cache_info.id.clone());
                    }
                }

                if cached_content_id.is_none() {
                    tracing::info!(
                        "Creating context cache for structured output ({} bytes)",
                        sys_str.len()
                    );
                    match self.create_context_cache(sys_msg).await {
                        Ok(id) => {
                            *cache_guard = Some(CachedContentInfo {
                                id: id.clone(),
                                hash: current_hash,
                            });
                            cached_content_id = Some(id);
                        }
                        Err(e) => {
                            tracing::warn!("Failed to create context cache: {}", e);
                        }
                    }
                }
            }
        }

        let final_system_instruction = if cached_content_id.is_some() {
            None
        } else {
            system_instruction.clone()
        };

        let generation_config = GenerationConfig {
            temperature: Some(0.0),
            max_output_tokens: Some(8192),
            thinking_config: None,
            response_mime_type: Some("application/json".to_string()),
            response_schema: Some(response_schema),
        };

        let req_body = GeminiRequest {
            contents: messages,
            system_instruction: final_system_instruction,
            tools: None,
            tool_config: None,
            generation_config: Some(generation_config),
            cached_content: cached_content_id,
        };

        let url = match self.platform {
            GeminiPlatform::Gen => format!(
                "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent", self.model_name
            ),
            GeminiPlatform::Vertex => format!(
                "https://aiplatform.googleapis.com/v1beta1/publishers/google/models/{}:generateContent", self.model_name
            ),
        };

        let mut attempts = 0;
        let max_attempts = 5;
        let mut last_error = String::from("initialization");

        let response_json = loop {
            attempts += 1;
            let response = match self.platform {
                GeminiPlatform::Gen => {
                    self.client
                        .post(&url)
                        .header(CONTENT_TYPE, "application/json")
                        .header("x-goog-api-key", self.api_key.clone())
                        .json(&req_body)
                        .send()
                        .await
                }
                GeminiPlatform::Vertex => {
                    let vertex_req = VertexGeminiRequest {
                        contents: req_body.contents.iter().map(to_vertex_message).collect(),
                        system_instruction: req_body
                            .system_instruction
                            .as_ref()
                            .map(to_vertex_message),
                        tools: req_body.tools.clone(),
                        tool_config: req_body.tool_config.clone(),
                        generation_config: req_body.generation_config.clone(),
                        cached_content: req_body.cached_content.clone(),
                    };
                    self.client
                        .post(&url)
                        .header(CONTENT_TYPE, "application/json")
                        .header("x-goog-api-key", self.api_key.clone())
                        .json(&vertex_req)
                        .send()
                        .await
                }
            };

            match response {
                Ok(r) if r.status().is_success() => {
                    break r.json::<Value>().await?;
                }
                Ok(r) => {
                    let status = r.status();
                    let error_text = r.text().await.unwrap_or_default();
                    last_error =
                        format!("status={} body={}", status, truncate_log_error(&error_text));
                    tracing::warn!(
                        "Gemini Structured API Error (Attempt {}/{}): {}",
                        attempts,
                        max_attempts,
                        last_error
                    );

                    let is_transient = status.is_server_error() || status.as_u16() == 429;
                    if !is_transient || attempts >= max_attempts {
                        return Err(LlmError::ApiError(last_error));
                    }
                }
                Err(e) => {
                    last_error = format_full_error(&e);
                    tracing::warn!(
                        "Gemini Structured Network Error (Attempt {}/{}): {}",
                        attempts,
                        max_attempts,
                        last_error
                    );
                    if attempts >= max_attempts {
                        return Err(LlmError::NetworkError(e));
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(1000 * attempts)).await;
        };

        let text = response_json["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .unwrap_or("{}");

        let parsed: Value = serde_json::from_str(text)?;
        Ok(parsed)
    }
}

fn parse_function_call_basic(part: &Value) -> Option<FunctionCall> {
    let func_call = part.get("functionCall")?;
    let name = func_call.get("name")?.as_str()?.to_string();
    let args = func_call.get("args")?.clone();
    let id = func_call
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Some(FunctionCall { name, args, id })
}

fn capture_thought_signature(part: &Value) -> Option<String> {
    part.get("thoughtSignature")
        .or_else(|| part.get("thought_signature"))
        .and_then(|ts| ts.as_str())
        .map(|s| s.to_string())
}

fn inline_schema_refs(value: &mut Value, root: &Value, depth: usize) {
    if depth > 20 {
        return; // safeguard against infinite recursion
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
    #[allow(dead_code)]
    context_window: usize,
    reasoning_effort: Option<String>,
}

impl OpenAiCompatClient {
    async fn process_delta_json(
        json: Value,
        tx: &mpsc::Sender<StreamEvent>,
        active_tools: &mut std::collections::HashMap<usize, (String, String, Option<String>)>,
        index_map: &mut std::collections::HashMap<usize, usize>,
    ) {
        if let Some(choices) = json.get("choices").and_then(|v| v.as_array()) {
            for choice in choices {
                if let Some(delta) = choice.get("delta") {
                    // 1. Text content
                    if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
                        if !content.is_empty() {
                            let _ = tx.send(StreamEvent::Text(content.to_string())).await;
                        }
                    }
                    // 2. Reasoning/Thinking content (DeepSeek/DashScope format)
                    if let Some(reasoning) = delta.get("reasoning_content").and_then(|v| v.as_str())
                    {
                        if !reasoning.is_empty() {
                            let _ = tx.send(StreamEvent::Thought(reasoning.to_string())).await;
                        }
                    }
                    // 3. Tool calls
                    if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                        for tc in tool_calls {
                            let api_idx =
                                tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                            let new_id = tc.get("id").and_then(|v| v.as_str());

                            let mut storage_idx = api_idx;
                            if let Some(id) = new_id {
                                if let Some(&k) = active_tools
                                    .iter()
                                    .find(|(_, v)| v.2.as_deref() == Some(id))
                                    .map(|(k, _)| k)
                                {
                                    storage_idx = k;
                                } else {
                                    // New tool call starting
                                    storage_idx = active_tools.keys().max().unwrap_or(&0)
                                        + if active_tools.is_empty() { 0 } else { 1 };
                                    index_map.insert(api_idx, storage_idx);
                                }
                            } else {
                                storage_idx = *index_map.get(&api_idx).unwrap_or(&api_idx);
                            }

                            let entry = active_tools
                                .entry(storage_idx)
                                .or_insert_with(|| (String::new(), String::new(), None));

                            if let Some(id) = new_id {
                                entry.2 = Some(id.to_string());
                            }

                            if let Some(func) = tc.get("function") {
                                if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                                    if entry.0.is_empty() {
                                        entry.0.push_str(name);
                                    } else if !entry.0.contains(name) {
                                        entry.0.push_str(name);
                                    } else {
                                        entry.0 = name.to_string();
                                    }
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

    #[allow(dead_code)]
    pub fn new(
        api_key: String,
        base_url: String,
        model_name: String,
        provider_name: String,
    ) -> Self {
        Self {
            api_key,
            client: create_standard_client(Some(&base_url)),
            base_url,
            model_name,
            provider_name,
            context_window: 1_000_000,
            reasoning_effort: None,
        }
    }

    pub fn new_with_window(
        api_key: String,
        base_url: String,
        model_name: String,
        provider_name: String,
        context_window: usize,
        reasoning_effort: Option<String>,
    ) -> Self {
        let client = create_standard_client(Some(&base_url));
        Self {
            api_key,
            base_url,
            model_name,
            provider_name,
            client,
            context_window,
            reasoning_effort,
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
                let text = msg
                    .parts
                    .iter()
                    .find_map(|p| p.text.as_deref())
                    .unwrap_or("");
                let mut tool_calls = Vec::new();
                for part in &msg.parts {
                    if let Some(fc) = &part.function_call {
                        let call_id = fc
                            .id
                            .clone()
                            .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4().simple()));
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
                    "role": "assistant"
                });

                if !text.is_empty() {
                    message_json["content"] = serde_json::Value::String(text.to_string());
                }

                if !tool_calls.is_empty() {
                    message_json["tool_calls"] = serde_json::Value::Array(tool_calls);
                }
                openai_messages.push(message_json);
            } else if msg.role == "function" {
                for part in &msg.parts {
                    if let Some(fr) = &part.function_response {
                        openai_messages.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": fr.id.clone().unwrap_or_else(|| "unknown".to_string()),
                            "content": fr.response.to_string()
                        }));
                    }
                }
            }
        }

        let mut body = serde_json::json!({
            "model": self.model_name,
            "messages": openai_messages,
        });

        if let Some(effort) = &self.reasoning_effort {
            body["reasoning_effort"] = serde_json::Value::String(effort.clone());
        }

        let body_json = serde_json::to_string(&body).unwrap_or_default();
        tracing::info!(
            "OpenAI generate_text request: url={}, body_size={} bytes",
            self.base_url,
            body_json.len()
        );
        tracing::debug!("OpenAI generate_text body: {}", truncate_log(&body_json));
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
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Could not read error body".to_string());
            let truncated_error = truncate_log_error(&error_text);
            tracing::error!(
                "OpenAI API Error: status={}, url={}, body={}",
                status,
                self.base_url,
                truncated_error
            );
            return Err(LlmError::ApiError(format!(
                "OpenAI API status={}: {}",
                status, truncated_error
            )));
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
                let text = msg
                    .parts
                    .iter()
                    .find_map(|p| p.text.as_deref())
                    .unwrap_or("");
                let mut tool_calls = Vec::new();
                for part in &msg.parts {
                    if let Some(fc) = &part.function_call {
                        let call_id = fc
                            .id
                            .clone()
                            .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4().simple()));
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
                    "role": "assistant"
                });

                if !text.is_empty() {
                    message_json["content"] = serde_json::Value::String(text.to_string());
                }

                if !tool_calls.is_empty() {
                    message_json["tool_calls"] = serde_json::Value::Array(tool_calls);
                }
                openai_messages.push(message_json);
            } else if msg.role == "function" {
                for part in &msg.parts {
                    if let Some(fr) = &part.function_response {
                        openai_messages.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": fr.id.clone().unwrap_or_else(|| "unknown".to_string()),
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

        if let Some(effort) = &self.reasoning_effort {
            body_map["reasoning_effort"] = serde_json::Value::String(effort.clone());
        }

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
            body_map["tool_choice"] = serde_json::Value::String("required".to_string());

            // Inject a final system prompt for autonomy
            openai_messages.push(serde_json::json!({
                "role": "system",
                "content": "CRITICAL FINAL REMINDER: You MUST output a tool call now unless the task is completely finished. Do NOT output conversational text asking for permission to continue."
            }));

            // Update body_map with the expanded messages array
            body_map["messages"] = serde_json::json!(openai_messages);
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
                    "Sending stream request to {} (Attempt {}/{}, body_size={} bytes)",
                    base_url,
                    attempts,
                    max_attempts,
                    body_json_string.len()
                );
                tracing::debug!("OpenAI stream body: {}", truncate_log(&body_json_string));

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
                        last_error =
                            format!("status={} body={}", status, truncate_log_error(&body));

                        tracing::warn!(
                            "OpenAI Stream API Error (Attempt {}/{}): {}",
                            attempts,
                            max_attempts,
                            last_error
                        );

                        if !is_transient || attempts >= max_attempts {
                            let _ = tx
                                .send(StreamEvent::Error(format!(
                                    "OpenAI API error after {} attempts: {}",
                                    attempts, last_error
                                )))
                                .await;
                            return;
                        }
                    }
                    Err(e) => {
                        last_error = format_full_error(&e);
                        tracing::warn!(
                            "OpenAI Network Error (Attempt {}/{}):\n{}",
                            attempts,
                            max_attempts,
                            last_error
                        );

                        if attempts >= max_attempts {
                            let _ = tx
                                .send(StreamEvent::Error(format!(
                                    "OpenAI network error after {} attempts: {}",
                                    attempts, last_error
                                )))
                                .await;
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
            let mut active_tools: std::collections::HashMap<
                usize,
                (String, String, Option<String>),
            > = std::collections::HashMap::new();
            let mut index_map: std::collections::HashMap<usize, usize> =
                std::collections::HashMap::new();

            while let Some(chunk_res) = stream.next().await {
                if let Ok(chunk) = chunk_res {
                    let chunk_str = String::from_utf8_lossy(&chunk);
                    tracing::debug!("Received OpenAI streaming chunk: {}", chunk_str);
                    buffer.push_str(&chunk_str);

                    // Process each line immediately (SSE standard uses single \n for data lines)
                    while let Some(idx) = buffer.find('\n') {
                        let line = buffer[..idx].trim().to_string();
                        buffer = buffer[idx + 1..].to_string();

                        if line.starts_with("data: ") {
                            let data = &line[6..];
                            if data == "[DONE]" {
                                tracing::debug!("OpenAI stream received [DONE]");
                                continue;
                            }
                            if let Ok(json) = serde_json::from_str::<Value>(data) {
                                OpenAiCompatClient::process_delta_json(
                                    json,
                                    &tx,
                                    &mut active_tools,
                                    &mut index_map,
                                )
                                .await;
                            }
                        }
                    }
                }
            }

            // Flush remaining partial data as a last resort
            if !buffer.trim().is_empty() {
                let line = buffer.trim();
                if line.starts_with("data: ") {
                    let data = &line[6..];
                    if data != "[DONE]" {
                        if let Ok(json) = serde_json::from_str::<Value>(data) {
                            OpenAiCompatClient::process_delta_json(
                                json,
                                &tx,
                                &mut active_tools,
                                &mut index_map,
                            )
                            .await;
                        }
                    }
                }
            }

            // Send all accumulated tool calls, sorted by index to preserve order
            let mut tool_indices: Vec<usize> = active_tools.keys().cloned().collect();
            tool_indices.sort_unstable();
            for idx in tool_indices {
                if let Some((name, args_str, id)) = active_tools.remove(&idx) {
                    if !name.trim().is_empty() {
                        let args = if args_str.trim().is_empty() {
                            serde_json::Value::Object(serde_json::Map::new())
                        } else {
                            serde_json::from_str(&args_str).unwrap_or(serde_json::Value::Null)
                        };
                        let final_id =
                            id.or_else(|| Some(format!("call_{}", uuid::Uuid::new_v4().simple())));
                        let _ = tx
                            .send(StreamEvent::ToolCall(
                                FunctionCall {
                                    name,
                                    args,
                                    id: final_id,
                                },
                                None,
                            ))
                            .await;
                    }
                }
            }

            let _ = tx.send(StreamEvent::Done).await;
        });
        Ok(rx)
    }

    async fn generate_structured(
        &self,
        _messages: Vec<Message>,
        _system_instruction: Option<Message>,
        _response_schema: Value,
    ) -> Result<Value, LlmError> {
        Err(LlmError::ApiError(
            "Structured output not yet implemented for OpenAI Compat".to_string(),
        ))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Part;
    use std::env;

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
                file_data: None,
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
