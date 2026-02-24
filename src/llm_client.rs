use crate::context::{FunctionCall, Message};
use futures::StreamExt;
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc;

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

            // Gemini schema parser is strict; simplify composition constructs.
            for combiner in ["anyOf", "oneOf", "allOf"] {
                if let Some(Value::Array(options)) = map.remove(combiner) {
                    let mut replacement = options
                        .into_iter()
                        .find(|candidate| !candidate.get("$ref").is_some())
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

#[derive(Error, Debug)]
pub enum LlmError {
    #[error("Network error: {0}")]
    NetworkError(#[from] reqwest::Error),
    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),
    #[error("API error: {0}")]
    ApiError(String),
}

pub struct GeminiClient {
    api_key: String,
    client: Client,
    model_name: String,
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

#[derive(Debug)]
pub enum StreamEvent {
    Text(String),
    ToolCall(FunctionCall),
    Error(String),
    Done,
}

fn parse_function_call(part: &Value) -> Option<FunctionCall> {
    let func_call = part.get("functionCall")?;
    let name = func_call
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let args = func_call.get("args").cloned().unwrap_or(Value::Null);
    let thought_signature = func_call
        .get("thought_signature")
        .or_else(|| func_call.get("thoughtSignature"))
        .and_then(|ts| ts.as_str())
        .map(|s| s.to_string());

    Some(FunctionCall {
        name,
        args,
        thought_signature,
    })
}

impl GeminiClient {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: Client::new(),
            model_name: "gemini-3.1-pro-preview".to_string(), // Or gemini-2.0-flash
        }
    }

    pub fn _set_model(&mut self, model: String) {
        self.model_name = model;
    }

    fn configured_models(&self) -> Vec<String> {
        let mut models = vec![self.model_name.clone()];
        if let Ok(fallbacks) = std::env::var("CLAW_FALLBACK_MODELS") {
            for model in fallbacks
                .split(',')
                .map(|m| m.trim())
                .filter(|m| !m.is_empty())
            {
                if !models.iter().any(|existing| existing == model) {
                    models.push(model.to_string());
                }
            }
        }
        models
    }

    pub async fn generate_text(
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

        let response = self
            .client
            .post(&url)
            .header(CONTENT_TYPE, "application/json")
            .json(&req_body)
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(LlmError::ApiError(format!(
                "API error: {}\nBody: {}",
                error_text, error_text
            )));
        }

        let resp_json: Value = response.json().await?;

        let text = resp_json
            .get("candidates")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .and_then(|c| c.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .and_then(|p| p.first())
            .and_then(|p| p.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

        Ok(text)
    }

    pub async fn stream(
        &self,
        messages: Vec<Message>,
        system_instruction: Option<Message>,
        tools: Vec<Arc<dyn crate::tools::Tool>>,
    ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
        let mut function_declarations = Vec::new();
        for tool in tools {
            let mut parameters = tool.parameters_schema();
            normalize_schema_for_gemini(&mut parameters);
            function_declarations.push(FunctionDeclaration {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters,
            });
        }

        let mut last_error: Option<String> = None;
        let response = {
            let mut selected_response = None;
            for model_name in self.configured_models() {
                let req_body = GeminiRequest {
                    contents: messages.clone(),
                    system_instruction: system_instruction.clone(),
                    tools: if function_declarations.is_empty() {
                        None
                    } else {
                        Some(vec![ToolDeclarationWrapper {
                            function_declarations: function_declarations.clone(),
                        }])
                    },
                    tool_config: None,
                };

                let url = format!(
                    "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?alt=sse&key={}",
                    model_name, self.api_key
                );

                if std::env::var("RUST_LOG").unwrap_or_default() == "debug" {
                    println!(
                        "Request body: {}",
                        serde_json::to_string_pretty(&req_body).unwrap()
                    );
                }

                match self
                    .client
                    .post(&url)
                    .header(CONTENT_TYPE, "application/json")
                    .json(&req_body)
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        selected_response = Some(resp);
                        break;
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        last_error = Some(format!(
                            "model={} status={} body={}",
                            model_name, status, body
                        ));
                    }
                    Err(e) => {
                        last_error = Some(format!("model={} transport={}", model_name, e));
                    }
                }
            }

            selected_response.ok_or_else(|| {
                LlmError::ApiError(format!(
                    "All configured models failed. Last error: {}",
                    last_error.unwrap_or_else(|| "unknown".to_string())
                ))
            })?
        };

        let (tx, rx) = mpsc::channel(100);

        let mut stream = response.bytes_stream();

        tokio::spawn(async move {
            let mut buffer = String::new();
            while let Some(chunk_res) = stream.next().await {
                match chunk_res {
                    Ok(chunk) => {
                        let text = String::from_utf8_lossy(&chunk);
                        buffer.push_str(&text);

                        if std::env::var("RUST_LOG").unwrap_or_default() == "debug" {
                            // println!("Buffer size: {}", buffer.len());
                        }

                        // Parse SSE lines
                        while let Some(idx) =
                            buffer.find("\r\n\r\n").or_else(|| buffer.find("\n\n"))
                        {
                            let _is_crlf = buffer[..idx].ends_with('\r');
                            let sep_len = if buffer.get(idx..idx + 4) == Some("\r\n\r\n") {
                                4
                            } else {
                                2
                            };

                            let line = buffer[..idx].to_string();
                            buffer = buffer[idx + sep_len..].to_string();

                            let mut data_str = line.as_str();
                            if data_str.starts_with("data: ") {
                                data_str = &data_str[6..];
                            } else {
                                continue;
                            }

                            if data_str == "[DONE]" {
                                let _ = tx.send(StreamEvent::Done).await;
                                return;
                            }
                            if std::env::var("RUST_LOG").unwrap_or_default() == "debug" {
                                println!("Raw SSE chunk: {}", data_str);
                            }

                            if let Ok(json) = serde_json::from_str::<Value>(data_str) {
                                if let Some(candidates) =
                                    json.get("candidates").and_then(|c| c.as_array())
                                {
                                    for cand in candidates {
                                        if let Some(parts) = cand
                                            .get("content")
                                            .and_then(|c| c.get("parts"))
                                            .and_then(|p| p.as_array())
                                        {
                                            for part in parts {
                                                if let Some(text) =
                                                    part.get("text").and_then(|t| t.as_str())
                                                {
                                                    let _ = tx
                                                        .send(StreamEvent::Text(text.to_string()))
                                                        .await;
                                                }
                                                if let Some(call) = parse_function_call(part) {
                                                    let _ =
                                                        tx.send(StreamEvent::ToolCall(call)).await;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                        break;
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

    #[test]
    fn normalize_schema_for_gemini_strips_unsupported_fields() {
        let mut value = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "definitions": {
                "Thing": {
                    "type": "object"
                }
            },
            "type": "object",
            "properties": {
                "nested": {
                    "$schema": "https://example.com/schema",
                    "type": ["string", "null"]
                },
                "refy": {
                    "$ref": "#/definitions/Thing"
                },
                "union": {
                    "anyOf": [
                        { "$ref": "#/definitions/Thing" },
                        { "type": "integer" }
                    ]
                }
            },
            "items": [
                {
                    "$schema": "https://example.com/schema2",
                    "type": "number"
                }
            ]
        });

        normalize_schema_for_gemini(&mut value);

        assert!(value.get("$schema").is_none());
        assert!(value.get("definitions").is_none());
        assert!(value["properties"]["nested"].get("$schema").is_none());
        assert_eq!(
            value["properties"]["nested"]
                .get("type")
                .and_then(|v| v.as_str()),
            Some("string")
        );
        assert_eq!(
            value["properties"]["refy"]
                .get("type")
                .and_then(|v| v.as_str()),
            Some("string")
        );
        assert_eq!(
            value["properties"]["union"]
                .get("type")
                .and_then(|v| v.as_str()),
            Some("integer")
        );
        assert!(value["items"][0].get("$schema").is_none());
    }

    #[test]
    fn parse_function_call_reads_snake_case_signature() {
        let part = serde_json::json!({
            "functionCall": {
                "name": "execute_bash",
                "args": { "command": "pwd" },
                "thought_signature": "sig_snake"
            }
        });

        let parsed = parse_function_call(&part).unwrap();
        assert_eq!(parsed.name, "execute_bash");
        assert_eq!(parsed.thought_signature.as_deref(), Some("sig_snake"));
    }

    #[test]
    fn parse_function_call_reads_camel_case_signature() {
        let part = serde_json::json!({
            "functionCall": {
                "name": "execute_bash",
                "args": { "command": "pwd" },
                "thoughtSignature": "sig_camel"
            }
        });

        let parsed = parse_function_call(&part).unwrap();
        assert_eq!(parsed.name, "execute_bash");
        assert_eq!(parsed.thought_signature.as_deref(), Some("sig_camel"));
    }

    #[test]
    fn test_regression_function_call_signature_compat() {
        let snake = serde_json::json!({
            "functionCall": {
                "name": "execute_bash",
                "args": { "command": "pwd" },
                "thought_signature": "sig_snake"
            }
        });
        let camel = serde_json::json!({
            "functionCall": {
                "name": "execute_bash",
                "args": { "command": "pwd" },
                "thoughtSignature": "sig_camel"
            }
        });

        assert_eq!(
            parse_function_call(&snake)
                .unwrap()
                .thought_signature
                .as_deref(),
            Some("sig_snake")
        );
        assert_eq!(
            parse_function_call(&camel)
                .unwrap()
                .thought_signature
                .as_deref(),
            Some("sig_camel")
        );
    }
}
