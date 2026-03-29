use async_trait::async_trait;
use rusty_claw::tools::{Tool, ToolContext, ToolError};
use rusty_claw::tools::protocol::StructuredToolOutput;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct MockTool {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub results: Arc<Mutex<Vec<Result<String, String>>>>,
    pub calls: Arc<Mutex<Vec<Value>>>,
}

impl MockTool {
    pub fn new(name: &str, result: Result<String, String>) -> Self {
        Self {
            name: name.to_string(),
            description: format!("Mock tool {}", name),
            parameters: serde_json::json!({
                "type": "OBJECT",
                "properties": {
                    "arg": { "type": "STRING" }
                }
            }),
            results: Arc::new(Mutex::new(vec![result])),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn with_results(name: &str, results: Vec<Result<String, String>>) -> Self {
        Self {
            name: name.to_string(),
            description: format!("Mock tool {}", name),
            parameters: serde_json::json!({
                "type": "OBJECT",
                "properties": {
                    "arg": { "type": "STRING" }
                }
            }),
            results: Arc::new(Mutex::new(results)),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl Tool for MockTool {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.parameters.clone()
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        self.calls.lock().await.push(args.clone());
        let mut results = self.results.lock().await;
        let res = if results.len() > 1 {
            results.remove(0)
        } else {
            results[0].clone()
        };

        match res {
            Ok(s) => {
                let mut output = StructuredToolOutput::new(&self.name, true, s.clone(), None, None, false);
                if self.name == "finish_task" {
                    output = output.with_finish_task_summary(s);
                }
                output.to_json_string()
            },
            Err(e) => Err(ToolError::ExecutionFailed(e)),
        }
    }
}

pub struct BlockingTool {
    pub name: String,
}

impl BlockingTool {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
        }
    }
}

#[async_trait]
impl Tool for BlockingTool {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn description(&self) -> String {
        "A tool that blocks forever".to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "OBJECT",
            "properties": {}
        })
    }

    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        // Block for a long time
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        Ok(StructuredToolOutput::new(&self.name, true, "done".to_string(), None, None, false).to_json_string().unwrap())
    }
}
