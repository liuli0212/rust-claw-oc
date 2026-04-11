use async_trait::async_trait;
use rusty_claw::context::{FunctionCall, Message};
use rusty_claw::llm_client::{LlmClient, LlmError, StreamEvent};
use rusty_claw::tools::Tool;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub enum ScenarioEvent {
    Text(String),
    #[allow(dead_code)]
    Thought(String),
    ToolCall(FunctionCall, Option<String>),
    #[allow(dead_code)]
    Error(String),
}

pub struct ScenarioTurn {
    pub events: Vec<ScenarioEvent>,
}

pub struct ScenarioLlm {
    turns: Arc<Mutex<Vec<ScenarioTurn>>>,
}

impl ScenarioLlm {
    pub fn new(turns: Vec<ScenarioTurn>) -> Self {
        Self {
            turns: Arc::new(Mutex::new(turns)),
        }
    }
}

#[async_trait]
impl LlmClient for ScenarioLlm {
    fn model_name(&self) -> &str {
        "scenario-model"
    }

    fn provider_name(&self) -> &str {
        "scenario-provider"
    }

    async fn stream(
        &self,
        _messages: Vec<Message>,
        _system_instruction: Option<Message>,
        _tools: Vec<Arc<dyn Tool>>,
    ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
        let (tx, rx) = mpsc::channel(100);
        let turns = self.turns.clone();

        tokio::spawn(async move {
            let mut turns_lock = turns.lock().await;
            if turns_lock.is_empty() {
                let _ = tx.send(StreamEvent::Done).await;
                return;
            }

            let turn = turns_lock.remove(0);
            for event in turn.events {
                let stream_event = match event {
                    ScenarioEvent::Text(t) => StreamEvent::Text(t),
                    ScenarioEvent::Thought(t) => StreamEvent::Thought(t),
                    ScenarioEvent::ToolCall(f, id) => StreamEvent::ToolCall(f, id),
                    ScenarioEvent::Error(e) => StreamEvent::Error(e),
                };
                if tx.send(stream_event).await.is_err() {
                    break;
                }
                // Yield to allow the receiver to process the event
                tokio::task::yield_now().await;
            }
            // Yield before sending Done
            tokio::task::yield_now().await;
            let _ = tx.send(StreamEvent::Done).await;
        });

        Ok(rx)
    }
}
