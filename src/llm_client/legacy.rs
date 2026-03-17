use crate::context::{FunctionCall, Message};
use crate::tools::Tool;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;

use super::gemini::{
    FunctionDeclaration, GeminiRequest, GenerationConfig, ThinkingConfig, ToolDeclarationWrapper,
};
use super::protocol::{GeminiPlatform, LlmClient, LlmError, StreamEvent};
use crate::utils::{format_full_error, truncate_log, truncate_log_error};

// --- Gemini Implementation ---

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

pub(super) fn create_standard_client(base_url: Option<&str>) -> Client {
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

#[cfg(test)]
mod tests {
    use crate::llm_client::policy::estimate_context_window;

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
