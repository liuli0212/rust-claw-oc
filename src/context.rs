use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Role {
    User,
    Model,
    Function,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Part {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none", rename = "functionCall")]
    pub function_call: Option<FunctionCall>,

    #[serde(skip_serializing_if = "Option::is_none", rename = "functionResponse")]
    pub function_response: Option<FunctionResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionResponse {
    pub name: String,
    pub response: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    #[serde(rename = "role")]
    pub role: String,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub turn_id: String,
    pub user_message: String,
    pub messages: Vec<Message>, // All messages in this turn (including tool calls/responses)
}

pub struct AgentContext {
    pub system_prompts: Vec<String>,
    pub dialogue_history: Vec<Turn>,
    pub current_turn: Option<Turn>,
}

impl AgentContext {
    pub fn new() -> Self {
        Self {
            system_prompts: vec![
                "You are Rusty-Claw, an industrial-grade Rust agent.".to_string(),
                "You have access to a set of tools. Use them to help the user.".to_string(),
                "Use the provided tools if needed. Always verify your actions.".to_string(),
            ],
            dialogue_history: Vec::new(),
            current_turn: None,
        }
    }

    pub fn start_turn(&mut self, text: String) {
        self.current_turn = Some(Turn {
            turn_id: uuid::Uuid::new_v4().to_string(),
            user_message: text.clone(),
            messages: vec![Message {
                role: "user".to_string(),
                parts: vec![Part {
                    text: Some(text),
                    function_call: None,
                    function_response: None,
                }],
            }],
        });
    }

    pub fn add_message_to_current_turn(&mut self, msg: Message) {
        if let Some(turn) = &mut self.current_turn {
            turn.messages.push(msg);
        }
    }

    pub fn end_turn(&mut self) {
        if let Some(turn) = self.current_turn.take() {
            self.dialogue_history.push(turn);
        }
    }

    pub fn build_llm_payload(&self) -> (Vec<Message>, Option<Message>) {
        let mut messages = Vec::new();

        let mut sys_text = String::new();
        for p in &self.system_prompts {
            sys_text.push_str(p);
            sys_text.push_str("\n\n");
        }

        let system_msg = Message {
            role: "system".to_string(),
            parts: vec![Part {
                text: Some(sys_text),
                function_call: None,
                function_response: None,
            }],
        };

        for turn in &self.dialogue_history {
            for msg in &turn.messages {
                messages.push(msg.clone());
            }
        }

        if let Some(turn) = &self.current_turn {
            for msg in &turn.messages {
                messages.push(msg.clone());
            }
        }

        (messages, Some(system_msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_turn_management() {
        let mut ctx = AgentContext::new();
        ctx.start_turn("Hello".to_string());
        assert!(ctx.current_turn.is_some());
        assert_eq!(ctx.current_turn.as_ref().unwrap().user_message, "Hello");
        
        ctx.add_message_to_current_turn(Message {
            role: "model".to_string(),
            parts: vec![Part { text: Some("Hi there".to_string()), function_call: None, function_response: None }]
        });
        
        ctx.end_turn();
        assert!(ctx.current_turn.is_none());
        assert_eq!(ctx.dialogue_history.len(), 1);
        assert_eq!(ctx.dialogue_history[0].messages.len(), 2);
    }
}
