use serde::{Deserialize, Serialize};
use tiktoken_rs::CoreBPE;

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
    #[allow(dead_code)]
    pub turn_id: String,
    #[allow(dead_code)]
    pub user_message: String,
    pub messages: Vec<Message>, // All messages in this turn (including tool calls/responses)
}

pub struct AgentContext {
    pub system_prompts: Vec<String>,
    pub dialogue_history: Vec<Turn>,
    pub current_turn: Option<Turn>,
    pub max_history_tokens: usize,
}

impl AgentContext {
    pub fn new() -> Self {
        Self {
            system_prompts: vec![
                "You are Rusty-Claw, an industrial-grade Rust agent.".to_string(),
                "You have access to a set of tools. Use them to help the user.".to_string(),
                "You are a local agent. The ability to interact with the local file system is your most important tool. You CAN and MUST run bash commands, read files, write files, and explore the local file system using your `execute_bash` tool.".to_string(),
                "NEVER say you cannot access the local file system. If asked to write a script or analyze code, DO IT using your tools.".to_string(),
                "Use the provided tools if needed. Always verify your actions.".to_string(),
            ],
            dialogue_history: Vec::new(),
            current_turn: None,
            max_history_tokens: 32000,
        }
    }

    fn estimate_tokens(bpe: &CoreBPE, msg: &Message) -> usize {
        let mut count = 0;
        for part in &msg.parts {
            if let Some(text) = &part.text {
                count += bpe.encode_with_special_tokens(text).len();
            }
            if let Some(fc) = &part.function_call {
                count += bpe.encode_with_special_tokens(&fc.name).len();
                count += bpe.encode_with_special_tokens(&fc.args.to_string()).len();
            }
            if let Some(fr) = &part.function_response {
                count += bpe.encode_with_special_tokens(&fr.name).len();
                count += bpe.encode_with_special_tokens(&fr.response.to_string()).len();
            }
        }
        count
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

        let bpe = tiktoken_rs::cl100k_base().unwrap();
        let mut history_messages = Vec::new();
        let mut current_tokens = 0;
        
        for turn in self.dialogue_history.iter().rev() {
            let turn_tokens: usize = turn.messages.iter().map(|m| Self::estimate_tokens(&bpe, m)).sum();
            if current_tokens + turn_tokens > self.max_history_tokens {
                println!("\n>> [Memory]: Working memory truncated due to token budget ({} / {})", current_tokens, self.max_history_tokens);
                break;
            }
            current_tokens += turn_tokens;
            
            let mut turn_block = Vec::new();
            for msg in &turn.messages {
                turn_block.push(msg.clone());
            }
            history_messages.push(turn_block);
        }
        history_messages.reverse();
        for block in history_messages { messages.extend(block); }

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
    
    #[test]
    fn test_token_budget_truncation() {
        let mut ctx = AgentContext::new();
        ctx.max_history_tokens = 10; // Extremely small budget to guarantee cutoff
        
        // Turn 1 (Oldest)
        ctx.start_turn("This is a very long string that should be truncated eventually. It has many many words and will exceed fifty tokens quickly.".to_string());
        ctx.end_turn();
        
        // Turn 2 (Newest)
        ctx.start_turn("Short message".to_string());
        ctx.end_turn();
        
        let (payload, _) = ctx.build_llm_payload();
        
        // Output should have 1 item from Turn 2. The Turn 1 should be dropped.
        // Actually wait, build_llm_payload also returns the CURRENT turn which is empty if we called end_turn, but let's check length
        assert!(payload.len() == 1, "Payload length was {}, expected 1", payload.len());
        assert_eq!(payload.last().unwrap().parts[0].text.as_ref().unwrap(), "Short message");
    }
}
