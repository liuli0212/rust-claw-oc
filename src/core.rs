use crate::context::{AgentContext, FunctionResponse, Message, Part};
use crate::llm_client::{GeminiClient, StreamEvent};
use crate::tools::Tool;
use crate::rag::VectorStore;
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
    rag_store: Option<Arc<VectorStore>>,
}

impl AgentLoop {
    pub fn new(
        llm: Arc<GeminiClient>,
        tools: Vec<Arc<dyn Tool>>,
        context: AgentContext,
        output: Arc<dyn AgentOutput>,
        rag_store: Option<Arc<VectorStore>>,
    ) -> Self {
        Self {
            llm,
            tools,
            context,
            output,
            rag_store,
        }
    }

    pub async fn step(&mut self, user_input: String) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Auto-RAG Pipeline
        if let Some(store) = &self.rag_store {
            if let Ok(results) = store.search(&user_input, 3) {
                let relevant: Vec<String> = results.into_iter()
                    .filter(|(_, _, score)| *score > 0.6)
                    .map(|(content, source, _)| format!("[Source: {}]\n{}", source, content))
                    .collect();
                
                if !relevant.is_empty() {
                    self.context.auto_rag_results = Some(relevant.join("\n---\n"));
                } else {
                    self.context.auto_rag_results = None;
                }
            }
        }

        self.context.start_turn(user_input);

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
                            Err(e) => {
                                let error_msg = e.to_string();
                                let hint = if error_msg.contains("No such file") || error_msg.contains("not found") {
                                    "Check if the file or directory exists. Use 'ls -R' to find files or 'pwd' to check your location."
                                } else if error_msg.contains("Permission denied") {
                                    "You might not have permission to access this resource."
                                } else if error_msg.contains("Timeout") {
                                    "The command took too long. Try a simpler command or increase the timeout."
                                } else {
                                    "Review your command syntax and arguments."
                                };
                                serde_json::json!({
                                    "error": error_msg,
                                    "hint": hint
                                }).to_string()
                            }
                        }
                    } else {
                        serde_json::json!({
                            "error": format!("Tool '{}' not found", tool_name),
                            "hint": "Check the available tools list and spelling."
                        }).to_string()
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
