use crate::context::{FunctionCall, Message};
use futures::StreamExt;
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
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

pub struct GeminiClient {
    api_key: String,
    client: Client,
    model_name: String,
}

#[derive(Serialize, Deserialize)]
pub struct GeminiRequest {
    pub contents: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "systemInstruction")]
    pub system_instruction: Option<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDeclarationWrapper>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "toolConfig")]
    pub tool_config: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize)]
pub struct ToolDeclarationWrapper {
    #[serde(rename = "functionDeclarations")]
    pub function_declarations: Vec<FunctionDeclaration>,
}

#[derive(Serialize, Deserialize)]
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
            function_declarations.push(FunctionDeclaration {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.parameters_schema(),
            });
        }

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
            self.model_name, self.api_key
        );
        
        if std::env::var("RUST_LOG").unwrap_or_default() == "debug" {
            println!("Request body: {}", serde_json::to_string_pretty(&req_body).unwrap());
        }

        let response = self
            .client
            .post(&url)
            .header(CONTENT_TYPE, "application/json")
            .json(&req_body)
            .send()
            .await?;

        if !response.status().is_success() {
            let err_text = response.text().await.unwrap_or_default();
            return Err(LlmError::ApiError(format!(
                "Status: {}, Body: {}",
                err_text, err_text
            ))); // Fixed formatting
        }

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
                        while let Some(idx) = buffer.find("\r\n\r\n").or_else(|| buffer.find("\n\n")) {
                            let _is_crlf = buffer[..idx].ends_with('\r');
                            let sep_len = if buffer.get(idx..idx+4) == Some("\r\n\r\n") { 4 } else { 2 };

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
                                                if let Some(func_call) = part.get("functionCall") {
                                                    let name = func_call
                                                        .get("name")
                                                        .and_then(|n| n.as_str())
                                                        .unwrap_or("")
                                                        .to_string();
                                                    let args = func_call
                                                        .get("args")
                                                        .cloned()
                                                        .unwrap_or(Value::Null);
                                                    let thought_signature = func_call
                                                        .get("thought_signature")
                                                        .and_then(|ts| ts.as_str())
                                                        .map(|s| s.to_string());
                                                        
                                                    let _ =
                                                        tx.send(StreamEvent::ToolCall(
                                                            FunctionCall { name, args, thought_signature },
                                                        ))
                                                        .await;
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
