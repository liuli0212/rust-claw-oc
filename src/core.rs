use crate::context::{AgentContext, FunctionResponse, Message, Part};
use crate::llm_client::{GeminiClient, StreamEvent};
use crate::tools::Tool;
use std::sync::Arc;

pub struct AgentLoop {
    llm: Arc<GeminiClient>,
    tools: Vec<Arc<dyn Tool>>,
    context: AgentContext,
}

impl AgentLoop {
    pub fn new(llm: Arc<GeminiClient>, tools: Vec<Arc<dyn Tool>>, context: AgentContext) -> Self {
        Self {
            llm,
            tools,
            context,
        }
    }

    pub async fn step(&mut self, user_input: String) -> Result<(), Box<dyn std::error::Error>> {
        self.context.start_turn(user_input);

        loop {
            let (history, system_instruction) = self.context.build_llm_payload();

            let mut rx = self
                .llm
                .stream(history.clone(), system_instruction, self.tools.clone())
                .await?;

            let mut full_text = String::new();
            let mut tool_calls = Vec::new();

            print!("Rusty-Claw: ");
            use std::io::Write;
            std::io::stdout().flush()?;

            while let Some(event) = rx.recv().await {
                match event {
                    StreamEvent::Text(text) => {
                        print!("{}", text);
                        std::io::stdout().flush()?;
                        full_text.push_str(&text);
                    }
                    StreamEvent::ToolCall(call) => {
                        tool_calls.push(call);
                    }
                    StreamEvent::Error(e) => {
                        println!("\n[LLM Error]: {}", e);
                        return Err(e.into());
                    }
                    StreamEvent::Done => break,
                }
            }
            println!();

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

                println!("\n> [Tool Call]: {} (args: {})", tool_name, tool_args);

                let result_str =
                    if let Some(tool) = self.tools.iter().find(|t| t.name() == tool_name) {
                        match tool.execute(tool_args.clone()).await {
                            Ok(res) => res,
                            Err(e) => format!("Error: {}", e),
                        }
                    } else {
                        format!("Error: Tool '{}' not found", tool_name)
                    };

                println!("> [Tool Result]: {}", result_str);

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
