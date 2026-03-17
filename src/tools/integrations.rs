use super::protocol::{clean_schema, Tool, ToolError};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub struct SendTelegramMessageTool {
    pub bot_token: String,
    client: reqwest::Client,
}

impl SendTelegramMessageTool {
    pub fn new(bot_token: String) -> Self {
        Self {
            bot_token,
            client: reqwest::Client::new(),
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SendTelegramMessageArgs {
    pub chat_id: String,
    pub text: String,
}

#[async_trait]
impl Tool for SendTelegramMessageTool {
    fn name(&self) -> String {
        "send_telegram_message".to_string()
    }

    fn description(&self) -> String {
        "Sends a direct Telegram message to a specific chat ID. Useful for notifications from external triggers."
            .to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schemars::schema_for!(SendTelegramMessageArgs)).unwrap())
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let parsed: SendTelegramMessageArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        if !parsed
            .chat_id
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-')
        {
            return Err(ToolError::InvalidArguments(
                "chat_id must be a numeric Telegram chat ID".to_string(),
            ));
        }

        if parsed.text.len() > 4096 {
            return Err(ToolError::InvalidArguments(
                "message text exceeds Telegram's 4096 character limit".to_string(),
            ));
        }

        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);
        let resp = self
            .client
            .post(&url)
            .form(&[("chat_id", &parsed.chat_id), ("text", &parsed.text)])
            .send()
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("Network error: {}", e)))?;

        let status = resp.status();
        if status.is_success() {
            Ok(format!(
                "Message successfully sent to Telegram chat ID {}",
                parsed.chat_id
            ))
        } else {
            let err_body = resp.text().await.unwrap_or_default();
            Err(ToolError::ExecutionFailed(format!(
                "Telegram API error ({}): {}",
                status, err_body
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_send_telegram_message_tool_validates_inputs_before_network() {
        let tool = SendTelegramMessageTool::new("fake-token".to_string());

        let invalid_chat = tool
            .execute(serde_json::json!({
                "chat_id": "abc123",
                "text": "hello"
            }))
            .await
            .unwrap_err();
        assert!(invalid_chat
            .to_string()
            .contains("chat_id must be a numeric Telegram chat ID"));

        let long_text = tool
            .execute(serde_json::json!({
                "chat_id": "12345",
                "text": "x".repeat(4097)
            }))
            .await
            .unwrap_err();
        assert!(long_text
            .to_string()
            .contains("message text exceeds Telegram's 4096 character limit"));
    }
}
