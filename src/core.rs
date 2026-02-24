use crate::context::{AgentContext, FunctionResponse, Message, Part};
use crate::llm_client::{GeminiClient, StreamEvent};
use crate::tools::Tool;
use std::sync::Arc;
use async_trait::async_trait;

#[async_trait]
pub trait AgentOutput: Send + Sync {
    async fn on_text(&self, text: &str);
    async fn on_tool_start(&self, name: &str, args: &str);
    async fn on_tool_end(&self, result: &str);
    async fn on_error(&self, error: &str);
}

pub struct AgentLoop {
    llm: Arc<GeminiClient>,
    tools: Vec<Arc<dyn Tool>>,
    context: AgentContext,
    output: Arc<dyn AgentOutput>,
}

impl AgentLoop {
    pub fn new(llm: Arc<GeminiClient>, tools: Vec<Arc<dyn Tool>>, context: AgentContext, output: Arc<dyn AgentOutput>) -> Self {
        Self {
            llm,
            tools,
            context,
            output,
        }
    }


    pub async fn step(&mut self, user_input: String) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.context.start_turn(user_input);

        // --- AUTO-COMPACTION LOGIC ---
        // If history has more than 10 turns, we compact the oldest 5 into a single summary
        if self.context.dialogue_history.len() > 10 {
            self.output.on_text("\n[System: Auto-compacting long conversation history to preserve context window...]\n").await;
            
            let oldest_turns: Vec<_> = self.context.dialogue_history.drain(0..5).collect();
            
            // Build a prompt for the LLM to summarize
            let mut summary_prompt = "Please summarize the following conversation history into a concise but comprehensive memorandum. Retain all key technical facts, file paths mentioned, decisions made, and pending issues.\n\n".to_string();
            
            for (i, turn) in oldest_turns.iter().enumerate() {
                summary_prompt.push_str(&format!("--- Turn {} ---\n", i + 1));
                summary_prompt.push_str(&format!("User: {}\n", turn.user_message));
                for msg in &turn.messages {
                    if msg.role == "model" {
                        for part in &msg.parts {
                            if let Some(text) = &part.text {
                                summary_prompt.push_str(&format!("Agent: {}\n", text));
                            }
                            if let Some(fc) = &part.function_call {
                                summary_prompt.push_str(&format!("Agent called tool '{}'\n", fc.name));
                            }
                        }
                    } else if msg.role == "function" {
                         summary_prompt.push_str("Agent received tool results.\n");
                    }
                }
                summary_prompt.push_str("\n");
            }

            let sys_msg = Message {
                role: "system".to_string(),
                parts: vec![Part {
                    text: Some("You are an expert summarization agent. Your job is to compress conversation history without losing technical details.".to_string()),
                    function_call: None,
                    function_response: None,
                }],
            };

            let user_msg = Message {
                role: "user".to_string(),
                parts: vec![Part {
                    text: Some(summary_prompt),
                    function_call: None,
                    function_response: None,
                }],
            };

            match self.llm.generate_text(vec![user_msg], Some(sys_msg)).await {
                Ok(summary) => {
                    // Create a synthetic turn representing the compacted history
                    let compacted_turn = crate::context::Turn {
                        turn_id: uuid::Uuid::new_v4().to_string(),
                        user_message: "SYSTEM: Old conversation history".to_string(),
                        messages: vec![
                            Message {
                                role: "user".to_string(),
                                parts: vec![Part {
                                    text: Some("What happened earlier?".to_string()),
                                    function_call: None,
                                    function_response: None,
                                }],
                            },
                            Message {
                                role: "model".to_string(),
                                parts: vec![Part {
                                    text: Some(format!("Earlier conversation summary:\n{}", summary)),
                                    function_call: None,
                                    function_response: None,
                                }],
                            }
                        ]
                    };
                    self.context.dialogue_history.insert(0, compacted_turn);
                    self.output.on_text("[System: Compaction complete.]\n\n").await;
                }
                Err(e) => {
                    self.output.on_error(&format!("\n[Compaction Error]: {}\n", e)).await;
                    // Put them back if failed
                    for (i, turn) in oldest_turns.into_iter().enumerate() {
                        self.context.dialogue_history.insert(i, turn);
                    }
                }
            }
        }
        // --- END AUTO-COMPACTION ---

        loop {

            let (history, system_instruction) = self.context.build_llm_payload();

            let mut rx = self
                .llm
                .stream(history.clone(), system_instruction, self.tools.clone())
                .await?;

            let mut full_text = String::new();
            let mut tool_calls = Vec::new();

            // print!("Rusty-Claw: ");
            // use std::io::Write;
            // std::io::stdout().flush()?;

            while let Some(event) = rx.recv().await {
                match event {
                    StreamEvent::Text(text) => {
                        self.output.on_text(&text).await;
                        // print!("{}", text);
                        // std::io::stdout().flush()?;
                        full_text.push_str(&text);
                    }
                    StreamEvent::ToolCall(call) => {
                        tool_calls.push(call);
                    }
                    StreamEvent::Error(e) => {
                        self.output.on_error(&format!("\n[LLM Error]: {}", e)).await;
                        // println!("\n[LLM Error]: {}", e);
                        return Err(e.into());
                    }
                    StreamEvent::Done => break,
                }
            }
            // println!();

            // Record assistant message
            let mut parts = Vec::new();
            if !full_text.is_empty() {
                parts.push(Part {
                    text: Some(full_text.clone()),
                    function_call: None,
                    function_response: None,
                });
            }
            for call in &tool_calls {
                parts.push(Part {
                    text: None,
                    function_call: Some(call.clone()),
                    function_response: None,
                });
            }
            if !parts.is_empty() {
                self.context.add_message_to_current_turn(Message {
                    role: "model".to_string(),
                    parts,
                });
            }

            if tool_calls.is_empty() {
                // Done with this turn
                break;
            }

            // Execute tools
            let mut response_parts = Vec::new();
            for call in tool_calls {
                let tool_name = call.name.clone();
                let tool_args = call.args.clone();

                self.output.on_tool_start(&tool_name, &tool_args.to_string()).await;
                // println!("\n> [Tool Call]: {} (args: {})", tool_name, tool_args);

                let result_str =
                    if let Some(tool) = self.tools.iter().find(|t| t.name() == tool_name) {
                        match tool.execute(tool_args.clone()).await {
                            Ok(res) => res,
                            Err(e) => format!("Error: {}", e),
                        }
                    } else {
                        format!("Error: Tool '{}' not found", tool_name)
                    };

                self.output.on_tool_end(&result_str).await;
                // println!("> [Tool Result]: {}", result_str);

                response_parts.push(Part {
                    text: None,
                    function_call: None,
                    function_response: Some(FunctionResponse {
                        name: tool_name,
                        response: serde_json::json!({ "result": result_str }),
                    }),
                });
            }

            self.context.add_message_to_current_turn(Message {
                role: "function".to_string(),
                parts: response_parts,
            });

            // Loop back to give LLM the tool results
        }

        self.context.end_turn();
        Ok(())
    }
}
