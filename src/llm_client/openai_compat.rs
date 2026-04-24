use crate::context::{FunctionCall, Message};
use crate::tools::Tool;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::Client;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::Instrument;

use super::protocol::{create_standard_client, LlmCapabilities, LlmClient, LlmError, StreamEvent};
use crate::utils::{format_full_error, truncate_log, truncate_log_error};

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
                    if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
                        if !content.is_empty() {
                            let _ = tx.send(StreamEvent::Text(content.to_string())).await;
                        }
                    }
                    if let Some(reasoning) = delta.get("reasoning_content").and_then(|v| v.as_str())
                    {
                        if !reasoning.is_empty() {
                            let _ = tx.send(StreamEvent::Thought(reasoning.to_string())).await;
                        }
                    }
                    if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                        for tc in tool_calls {
                            let api_idx =
                                tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                            let new_id = tc.get("id").and_then(|v| v.as_str());

                            let storage_idx = if let Some(id) = new_id {
                                if let Some(&k) = active_tools
                                    .iter()
                                    .find(|(_, v)| v.2.as_deref() == Some(id))
                                    .map(|(k, _)| k)
                                {
                                    k
                                } else {
                                    let next_idx = active_tools.keys().max().unwrap_or(&0)
                                        + if active_tools.is_empty() { 0 } else { 1 };
                                    index_map.insert(api_idx, next_idx);
                                    next_idx
                                }
                            } else {
                                *index_map.get(&api_idx).unwrap_or(&api_idx)
                            };

                            let entry = active_tools
                                .entry(storage_idx)
                                .or_insert_with(|| (String::new(), String::new(), None));

                            if let Some(id) = new_id {
                                entry.2 = Some(id.to_string());
                            }

                            if let Some(func) = tc.get("function") {
                                if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                                    if entry.0.is_empty() || !entry.0.contains(name) {
                                        entry.0.push_str(name);
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
    fn context_window(&self) -> usize {
        self.context_window
    }
    fn capabilities(&self) -> LlmCapabilities {
        LlmCapabilities {
            function_tools: true,
            custom_tools: false,
            parallel_tool_calls: true,
            supports_code_mode: true,
        }
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
            "parallel_tool_calls": true,
        });

        if let Some(effort) = &self.reasoning_effort {
            body_map["reasoning_effort"] = serde_json::Value::String(effort.clone());
        }

        if !tools.is_empty() {
            let mut openai_tools = Vec::new();
            for tool in tools {
                let definition = tool.definition();
                openai_tools.push(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": definition.name,
                        "description": definition.description,
                        "parameters": definition.input_schema.unwrap_or_else(|| serde_json::json!({ "type": "object", "properties": {} })),
                    }
                }));
            }
            if !openai_tools.is_empty() {
                body_map["tools"] = serde_json::json!(openai_tools);
                body_map["tool_choice"] = serde_json::Value::String("required".to_string());

                openai_messages.push(serde_json::json!({
                    "role": "system",
                    "content": "CRITICAL FINAL REMINDER: You MUST output a tool call now unless the task is completely finished. Do NOT output conversational text asking for permission to continue."
                }));

                body_map["messages"] = serde_json::json!(openai_messages);
            }
        }

        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let base_url = self.base_url.clone();

        tokio::spawn(
            async move {
                let mut attempts = 0;
                let max_attempts = 5;

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
                            let last_error =
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
                            let last_error = format_full_error(&e);
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

                    let backoff = std::time::Duration::from_secs(1 << (attempts - 1));
                    tracing::info!("Transient error detected. Retrying in {:?}...", backoff);
                    tokio::time::sleep(backoff).await;
                };

                let mut stream = resp.bytes_stream();
                let mut buffer = String::new();
                let mut active_tools: std::collections::HashMap<
                    usize,
                    (String, String, Option<String>),
                > = std::collections::HashMap::new();
                let mut index_map: std::collections::HashMap<usize, usize> =
                    std::collections::HashMap::new();

                while let Some(chunk_res) = stream.next().await {
                    match chunk_res {
                        Ok(chunk) => {
                            let chunk_str = String::from_utf8_lossy(&chunk);
                            tracing::debug!("Received OpenAI streaming chunk: {}", chunk_str);
                            buffer.push_str(&chunk_str);

                            while let Some(idx) = buffer.find('\n') {
                                let line = buffer[..idx].trim().to_string();
                                buffer = buffer[idx + 1..].to_string();

                                if let Some(data) = line.strip_prefix("data: ") {
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
                        Err(e) => {
                            tracing::error!("OpenAI stream read error: {}", e);
                            let _ = tx
                                .send(StreamEvent::Error(format!("Stream read error: {}", e)))
                                .await;
                            return;
                        }
                    }
                }

                if !buffer.trim().is_empty() {
                    let line = buffer.trim();
                    if let Some(data) = line.strip_prefix("data: ") {
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
                            let final_id = id.or_else(|| {
                                Some(format!("call_{}", uuid::Uuid::new_v4().simple()))
                            });
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
            }
            .in_current_span(),
        );
        Ok(rx)
    }
}

#[cfg(test)]
impl OpenAiCompatClient {
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
                "OpenAI API error ({}): {}",
                status, truncated_error
            )));
        }

        let response_text = response.text().await?;
        let response_json: Value = serde_json::from_str(&response_text)?;
        let text = response_json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Part;
    use crate::llm_client::policy::estimate_context_window;
    use std::env;

    #[tokio::test]
    #[ignore]
    async fn test_aliyun_qwen_generate() {
        let _ = dotenvy::dotenv();
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
