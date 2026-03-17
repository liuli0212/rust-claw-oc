use super::agent_context::AgentContext;
use super::model::{Message, Part, Turn};

pub fn start_turn(ctx: &mut AgentContext, text: String) {
    ctx.current_turn = Some(Turn {
        turn_id: uuid::Uuid::new_v4().to_string(),
        user_message: text.clone(),
        messages: vec![Message {
            role: "user".to_string(),
            parts: vec![Part {
                text: Some(text),
                function_call: None,
                function_response: None,
                thought_signature: None,
                file_data: None,
            }],
        }],
    });
}

pub fn add_message_to_current_turn(ctx: &mut AgentContext, msg: Message) {
    if let Some(turn) = &mut ctx.current_turn {
        turn.messages.push(msg);
    }
}

pub fn end_turn(ctx: &mut AgentContext) {
    if let Some(turn) = ctx.current_turn.take() {
        if let Err(e) = ctx.append_turn_to_transcript(&turn) {
            tracing::warn!("Failed to append turn to transcript: {}", e);
        }
        ctx.dialogue_history.push(turn);
    }
}
