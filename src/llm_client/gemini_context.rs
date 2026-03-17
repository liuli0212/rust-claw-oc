use super::gemini::{inline_schema_refs, normalize_schema_for_gemini, FunctionDeclaration};
use super::protocol::LlmError;
use crate::context::{FileData, Message};
use crate::tools::Tool;
use reqwest::Client;
use std::sync::Arc;

pub(crate) fn build_function_declarations(tools: &[Arc<dyn Tool>]) -> Vec<FunctionDeclaration> {
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
        .header("X-Goog-Upload-Header-Content-Length", content.len().to_string())
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
