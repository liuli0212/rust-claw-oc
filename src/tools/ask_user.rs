//! AskUserQuestion tool — structured user interaction during skill execution.

use async_trait::async_trait;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::protocol::{clean_schema, StructuredToolOutput, Tool, ToolError, UserPromptRequest};

pub struct AskUserQuestionTool;

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AskUserQuestionArgs {
    /// The question to ask the user.
    pub question: String,
    /// A unique key to identify this question in the skill's answer store.
    pub context_key: String,
    /// Optional list of suggested options.
    #[serde(default)]
    pub options: Vec<String>,
    /// Optional recommended answer.
    pub recommendation: Option<String>,
}

impl AskUserQuestionTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for AskUserQuestionTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn name(&self) -> String {
        "ask_user_question".to_string()
    }

    fn description(&self) -> String {
        "Ask the user a structured question. Execution will pause until the user responds."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        clean_schema(serde_json::to_value(schema_for!(AskUserQuestionArgs)).unwrap())
    }

    fn has_side_effects(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &super::protocol::ToolContext,
    ) -> Result<String, ToolError> {
        let parsed: AskUserQuestionArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let prompt_request = UserPromptRequest {
            question: parsed.question.clone(),
            context_key: parsed.context_key.clone(),
            options: parsed.options.clone(),
            recommendation: parsed.recommendation.clone(),
        };

        // Build the visible output that the model sees as the tool result
        let mut output = format!("Question posed to user: {}", parsed.question);
        if !parsed.options.is_empty() {
            output.push_str(&format!("\nOptions: {}", parsed.options.join(", ")));
        }
        if let Some(rec) = &parsed.recommendation {
            output.push_str(&format!("\nRecommendation: {}", rec));
        }
        output.push_str("\n\n[Waiting for user response...]");

        let structured =
            StructuredToolOutput::new("ask_user_question", true, output, None, None, false)
                .with_await_user(prompt_request);

        structured.to_json_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::protocol::{ToolContext, ToolExecutionEnvelope};

    fn make_ctx() -> ToolContext {
        ToolContext::new("test", "test")
    }

    #[tokio::test]
    async fn test_ask_user_question_basic() {
        let tool = AskUserQuestionTool::new();
        let args = serde_json::json!({
            "question": "What is the project name?",
            "context_key": "project_name"
        });

        let result = tool.execute(args, &make_ctx()).await.unwrap();
        let envelope: ToolExecutionEnvelope = serde_json::from_str(&result).unwrap();

        assert!(envelope.result.ok);
        assert!(envelope.effects.await_user.is_some());
        let prompt = envelope.effects.await_user.unwrap();
        assert_eq!(prompt.question, "What is the project name?");
        assert_eq!(prompt.context_key, "project_name");
        assert!(prompt.options.is_empty());
    }

    #[tokio::test]
    async fn test_ask_user_question_with_options() {
        let tool = AskUserQuestionTool::new();
        let args = serde_json::json!({
            "question": "Choose a mode",
            "context_key": "mode",
            "options": ["startup", "growth", "enterprise"],
            "recommendation": "growth"
        });

        let result = tool.execute(args, &make_ctx()).await.unwrap();
        let envelope: ToolExecutionEnvelope = serde_json::from_str(&result).unwrap();

        let prompt = envelope.effects.await_user.unwrap();
        assert_eq!(prompt.options.len(), 3);
        assert_eq!(prompt.recommendation.as_deref(), Some("growth"));
    }

    #[tokio::test]
    async fn test_ask_user_has_no_side_effects() {
        let tool = AskUserQuestionTool::new();
        assert!(!tool.has_side_effects());
    }
}
