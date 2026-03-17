use crate::context::Message;
use crate::tools::Tool;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;

use super::gemini::{
    FunctionDeclaration, GeminiRequest, ToolDeclarationWrapper,
};
use super::gemini_context;
use super::protocol::{GeminiPlatform, LlmClient, LlmError, StreamEvent};
use crate::utils::{truncate_log, truncate_log_error};

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
pub(crate) struct CachedContentInfo {
    pub(crate) id: String,
    pub(crate) hash: u64,
}

struct CachedFunctionDeclarations {
    #[allow(dead_code)]
    signature: String,
    #[allow(dead_code)]
    declarations: Vec<FunctionDeclaration>,
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
        let generation_config = gemini_context::text_generation_config(&self.model_name);

        let req_body = GeminiRequest {
            contents: messages,
            system_instruction: final_system_instruction,
            tools: None,
            tool_config: None,
            generation_config: generation_config.clone(),
            cached_content: cached_content_id,
        };

        let req_body_json = serde_json::to_string(&req_body).unwrap_or_default();
        let url = gemini_context::request_url(self.platform, &self.model_name, false);

        tracing::info!(
            "Gemini generate_text request: url={}, body_size={} bytes",
            url,
            req_body_json.len()
        );
        tracing::debug!(
            "Gemini generate_text body: {}",
            truncate_log(&req_body_json)
        );

        let response = gemini_context::send_generate_request(
            &self.client,
            &self.api_key,
            self.platform,
            &url,
            &req_body,
            None,
        )
        .await?;

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

        tokio::spawn(async move {
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

        let cached_content_id = gemini_context::resolve_cached_content(
            &self.client,
            &self.api_key,
            &self.model_name,
            &self.cached_content,
            &system_instruction,
            "structured output",
        )
        .await;
        let final_system_instruction =
            gemini_context::final_system_instruction(&system_instruction, &cached_content_id);

        let generation_config = super::gemini::GenerationConfig {
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

        let url = gemini_context::request_url(self.platform, &self.model_name, false);

        let response_json = gemini_context::generate_with_retry(
            &self.client,
            &self.api_key,
            self.platform,
            &url,
            &req_body,
        )
        .await?;

        let text = response_json["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .unwrap_or("{}");

        let parsed: Value = serde_json::from_str(text)?;
        Ok(parsed)
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
