use super::gemini::{
    capture_thought_signature, inline_schema_refs, normalize_schema_for_gemini,
    parse_function_call_basic, to_vertex_message, CachedContentInfo, FunctionDeclaration,
    GeminiRequest, GenerationConfig, ThinkingConfig, VertexGeminiRequest,
};
use super::protocol::{GeminiPlatform, LlmError};
use crate::context::{FileData, Message};
use crate::tools::Tool;
use crate::utils::{format_full_error, truncate_log_error};
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;

pub(crate) fn build_function_declarations(tools: &[Arc<dyn Tool>]) -> Vec<FunctionDeclaration> {
    if tools.is_empty() {
        return Vec::new();
    }

    let mut declarations = Vec::with_capacity(tools.len());
    for tool in tools {
        let definition = tool.definition();
        if definition.kind != crate::tools::ToolKind::Function {
            continue;
        }
        let mut parameters = definition
            .input_schema
            .unwrap_or_else(|| serde_json::json!({ "type": "object", "properties": {} }));
        let root_schema = parameters.clone();
        inline_schema_refs(&mut parameters, &root_schema, 0);
        normalize_schema_for_gemini(&mut parameters);
        declarations.push(FunctionDeclaration {
            name: definition.name,
            description: definition.description,
            parameters,
        });
    }
    declarations
}

pub(crate) async fn upload_content(
    client: &Client,
    api_key: &str,
    content: &str,
    mime_type: &str,
) -> Result<String, LlmError> {
    let url = format!(
        "https://generativelanguage.googleapis.com/upload/v1beta/files?key={}",
        api_key
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

    let response = client
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

    let bytes = content.as_bytes();
    let chunk_size = 5 * 1024 * 1024;
    let total_len = bytes.len();
    let mut offset = 0;

    while offset < total_len {
        let end = (offset + chunk_size).min(total_len);
        let chunk = bytes[offset..end].to_vec();
        let is_last = end == total_len;

        let upload_response = client
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
            let final_uri = final_json["file"]["uri"].as_str().unwrap().to_string();
            return Ok(final_uri);
        }

        offset = end;
    }

    Err(LlmError::ApiError(
        "Upload finished without returning a file URI".to_string(),
    ))
}

pub(crate) async fn dehydrate_messages(
    client: &Client,
    api_key: &str,
    messages: &mut Vec<Message>,
) -> Result<(), LlmError> {
    for msg in messages {
        dehydrate_message(client, api_key, msg).await?;
    }
    Ok(())
}

pub(crate) async fn dehydrate_message(
    client: &Client,
    api_key: &str,
    msg: &mut Message,
) -> Result<(), LlmError> {
    for part in &mut msg.parts {
        if let Some(text) = &part.text {
            if text.len() > 512 * 1024 {
                let file_uri = upload_content(client, api_key, text, "text/plain").await?;
                part.text = None;
                part.file_data = Some(FileData {
                    mime_type: "text/plain".to_string(),
                    file_uri,
                });
            }
        }
    }
    Ok(())
}

pub(crate) async fn create_context_cache(
    client: &Client,
    api_key: &str,
    model_name: &str,
    system_instruction: &Message,
) -> Result<String, LlmError> {
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/cachedContents?key={}",
        api_key
    );

    let body = serde_json::json!({
        "model": format!("models/{}", model_name),
        "systemInstruction": system_instruction,
        "ttl": "3600s"
    });

    let response = client.post(&url).json(&body).send().await?;
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

pub(crate) async fn resolve_cached_content(
    client: &Client,
    api_key: &str,
    model_name: &str,
    cached_content: &Mutex<Option<CachedContentInfo>>,
    system_instruction: &Option<Message>,
    log_label: &str,
) -> Option<String> {
    let sys_msg = system_instruction.as_ref()?;
    let sys_str = serde_json::to_string(sys_msg).unwrap_or_default();
    if sys_str.len() <= 128 * 1024 {
        return None;
    }

    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    sys_str.hash(&mut hasher);
    let current_hash = hasher.finish();

    let mut cache_guard = cached_content.lock().await;
    if let Some(cache_info) = &*cache_guard {
        if cache_info.hash == current_hash {
            return Some(cache_info.id.clone());
        }
    }

    tracing::info!(
        "Creating context cache for {} ({} bytes)",
        log_label,
        sys_str.len()
    );
    match create_context_cache(client, api_key, model_name, sys_msg).await {
        Ok(id) => {
            *cache_guard = Some(CachedContentInfo {
                id: id.clone(),
                hash: current_hash,
            });
            Some(id)
        }
        Err(e) => {
            tracing::warn!("Failed to create context cache: {}", e);
            None
        }
    }
}

pub(crate) fn final_system_instruction(
    system_instruction: &Option<Message>,
    cached_content_id: &Option<String>,
) -> Option<Message> {
    if cached_content_id.is_some() {
        None
    } else {
        system_instruction.clone()
    }
}

pub(crate) fn text_generation_config(model_name: &str) -> Option<GenerationConfig> {
    if model_name.contains("thinking") {
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
    }
}

pub(crate) fn request_url(platform: GeminiPlatform, model_name: &str, streaming: bool) -> String {
    match (platform, streaming) {
        (GeminiPlatform::Gen, false) => format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
            model_name
        ),
        (GeminiPlatform::Vertex, false) => format!(
            "https://aiplatform.googleapis.com/v1beta1/publishers/google/models/{}:generateContent",
            model_name
        ),
        (GeminiPlatform::Gen, true) => format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?alt=sse",
            model_name
        ),
        (GeminiPlatform::Vertex, true) => format!(
            "https://aiplatform.googleapis.com/v1beta1/publishers/google/models/{}:streamGenerateContent?alt=sse",
            model_name
        ),
    }
}

pub(crate) fn to_vertex_request(
    req_body: &GeminiRequest,
    cached_content: Option<String>,
) -> VertexGeminiRequest {
    VertexGeminiRequest {
        contents: req_body.contents.iter().map(to_vertex_message).collect(),
        system_instruction: req_body.system_instruction.as_ref().map(to_vertex_message),
        tools: req_body.tools.clone(),
        tool_config: req_body.tool_config.clone(),
        generation_config: req_body.generation_config.clone(),
        cached_content,
    }
}

pub(crate) fn request_body_json(
    platform: GeminiPlatform,
    req_body: &GeminiRequest,
    vertex_cached_content: Option<String>,
) -> String {
    match platform {
        GeminiPlatform::Gen => serde_json::to_string(req_body).unwrap_or_default(),
        GeminiPlatform::Vertex => {
            let vertex_req = to_vertex_request(req_body, vertex_cached_content);
            serde_json::to_string(&vertex_req).unwrap_or_default()
        }
    }
}

#[allow(dead_code)]
pub(crate) async fn send_generate_request(
    client: &Client,
    api_key: &str,
    platform: GeminiPlatform,
    url: &str,
    req_body: &GeminiRequest,
    vertex_cached_content: Option<String>,
) -> Result<reqwest::Response, reqwest::Error> {
    match platform {
        GeminiPlatform::Gen => {
            client
                .post(url)
                .header(CONTENT_TYPE, "application/json")
                .header("x-goog-api-key", api_key)
                .json(req_body)
                .send()
                .await
        }
        GeminiPlatform::Vertex => {
            let vertex_req = to_vertex_request(req_body, vertex_cached_content);
            client
                .post(url)
                .header(CONTENT_TYPE, "application/json")
                .header("x-goog-api-key", api_key)
                .json(&vertex_req)
                .send()
                .await
        }
    }
}

#[allow(dead_code)]
pub(crate) async fn generate_with_retry(
    client: &Client,
    api_key: &str,
    platform: GeminiPlatform,
    url: &str,
    req_body: &GeminiRequest,
) -> Result<Value, LlmError> {
    let mut attempts = 0;
    let max_attempts = 5;
    loop {
        attempts += 1;
        let response = send_generate_request(
            client,
            api_key,
            platform,
            url,
            req_body,
            req_body.cached_content.clone(),
        )
        .await;

        match response {
            Ok(r) if r.status().is_success() => {
                return Ok(r.json::<Value>().await?);
            }
            Ok(r) => {
                let status = r.status();
                let error_text = r.text().await.unwrap_or_default();
                let last_error =
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
                let last_error = format_full_error(&e);
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
    }
}

#[allow(dead_code)]
pub(crate) async fn stream_connect_with_retry(
    client: &Client,
    api_key: &str,
    url: &str,
    body_json_string: &str,
) -> Result<reqwest::Response, String> {
    let mut attempts = 0;
    let max_attempts = 5;

    loop {
        attempts += 1;

        tracing::info!(
            "Sending Gemini stream request (Attempt {}/{}, body_size={} bytes)",
            attempts,
            max_attempts,
            body_json_string.len()
        );
        tracing::debug!(
            "Gemini stream body: {}",
            crate::utils::truncate_log(body_json_string)
        );

        let req_result = client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .header("x-goog-api-key", api_key)
            .body(body_json_string.to_string())
            .send()
            .await;

        match req_result {
            Ok(r) if r.status().is_success() => return Ok(r),
            Ok(r) => {
                let status = r.status();
                let is_transient = status.is_server_error() || status.as_u16() == 429;
                let body = r.text().await.unwrap_or_default();
                let last_error = format!("status={} body={}", status, truncate_log_error(&body));

                tracing::warn!(
                    "Gemini Stream API Error (Attempt {}/{}): {}",
                    attempts,
                    max_attempts,
                    last_error
                );

                if !is_transient || attempts >= max_attempts {
                    return Err(format!(
                        "Gemini API error after {} attempts: {}",
                        attempts, last_error
                    ));
                }
            }
            Err(e) => {
                let last_error = format_full_error(&e);
                tracing::warn!(
                    "Gemini Network Error (Attempt {}/{}):\n{}",
                    attempts,
                    max_attempts,
                    last_error
                );

                if attempts >= max_attempts {
                    return Err(format!(
                        "Gemini network error after {} attempts: {}",
                        attempts, last_error
                    ));
                }
            }
        }

        let backoff = std::time::Duration::from_secs(1 << (attempts - 1));
        tracing::info!("Transient error detected. Retrying in {:?}...", backoff);
        tokio::time::sleep(backoff).await;
    }
}

#[allow(dead_code)]
pub(crate) async fn emit_sse_data_block(
    tx: &mpsc::Sender<super::protocol::StreamEvent>,
    data: &str,
    total_text_len: &mut usize,
    total_tool_calls: &mut usize,
    chunk_count: &mut usize,
) -> bool {
    if data == "[DONE]" {
        tracing::debug!("Gemini stream received [DONE] signal");
        let _ = tx.send(super::protocol::StreamEvent::Done).await;
        return true;
    }

    match serde_json::from_str::<Value>(data) {
        Ok(json) => {
            *chunk_count += 1;
            tracing::debug!(
                "Gemini SSE chunk #{}: keys={:?}\nRaw: {}",
                *chunk_count,
                json.as_object().map(|m| m.keys().collect::<Vec<_>>()),
                crate::utils::truncate_log(data)
            );

            if let Some(candidate) = json["candidates"].as_array().and_then(|a| a.first()) {
                if let Some(thought) = candidate.get("thought").and_then(|v| v.as_str()) {
                    if !thought.is_empty() {
                        let _ = tx
                            .send(super::protocol::StreamEvent::Thought(thought.to_string()))
                            .await;
                    }
                }
                if let Some(thinking) = candidate.get("thinking").and_then(|v| v.as_str()) {
                    if !thinking.is_empty() {
                        let _ = tx
                            .send(super::protocol::StreamEvent::Thought(thinking.to_string()))
                            .await;
                    }
                }
            }

            if let Some(parts) = json["candidates"][0]["content"]["parts"].as_array() {
                let candidate_signature = json["candidates"][0]
                    .get("thoughtSignature")
                    .or_else(|| json["candidates"][0].get("thought_signature"))
                    .and_then(|ts| ts.as_str())
                    .map(|s| s.to_string());

                for part in parts {
                    tracing::trace!("Gemini part: {}", part);
                    let is_thought = part["thought"].as_bool().unwrap_or(false);

                    if let Some(thought_text) = part["thought"].as_str() {
                        let _ = tx
                            .send(super::protocol::StreamEvent::Thought(
                                thought_text.to_string(),
                            ))
                            .await;
                    }

                    if is_thought {
                        if let Some(text) = part["text"].as_str() {
                            let _ = tx
                                .send(super::protocol::StreamEvent::Thought(text.to_string()))
                                .await;
                            tracing::debug!(
                                "Gemini thought: {} chars, content={}",
                                text.len(),
                                crate::utils::truncate_log(text)
                            );
                        }
                    } else if let Some(text) = part["text"].as_str() {
                        *total_text_len += text.len();
                        tracing::debug!(
                            "Gemini text: {} chars (total: {}), content={}",
                            text.len(),
                            *total_text_len,
                            crate::utils::truncate_log(text)
                        );
                        let _ = tx
                            .send(super::protocol::StreamEvent::Text(text.to_string()))
                            .await;
                    }

                    if let Some(func_call) = parse_function_call_basic(part) {
                        *total_tool_calls += 1;
                        tracing::debug!("Gemini tool_call: name={}", func_call.name);
                        let signature =
                            capture_thought_signature(part).or_else(|| candidate_signature.clone());
                        let _ = tx
                            .send(super::protocol::StreamEvent::ToolCall(func_call, signature))
                            .await;
                    }
                }
            } else {
                tracing::debug!(
                    "Gemini SSE chunk #{} has no candidates/parts. Raw: {}",
                    *chunk_count,
                    crate::utils::truncate_log(data)
                );
            }
        }
        Err(parse_err) => {
            tracing::warn!(
                "Gemini SSE JSON parse error: {}. Raw data: {}",
                parse_err,
                crate::utils::truncate_log(data)
            );
        }
    }

    false
}
