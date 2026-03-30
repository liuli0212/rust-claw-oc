use async_trait::async_trait;
use rusty_claw::tools::protocol::{StructuredToolOutput, Tool, ToolContext, ToolError};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::Duration;

pub struct MockTool {
    pub name: String,
    pub results: Arc<Mutex<Vec<Result<String, String>>>>,
    pub calls: Arc<Mutex<Vec<Value>>>,
}

impl MockTool {
    pub fn new(name: &str, result: Result<String, String>) -> Self {
        Self {
            name: name.to_string(),
            results: Arc::new(Mutex::new(vec![result])),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn with_results(name: &str, results: Vec<Result<String, String>>) -> Self {
        Self {
            name: name.to_string(),
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
        format!("Mock tool for {}", self.name)
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "OBJECT",
            "properties": {
                "arg": {
                    "type": "STRING"
                }
            }
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        self.calls.lock().await.push(args);

        let mut results = self.results.lock().await;
        let result = if results.is_empty() {
            Ok("default mock result".to_string())
        } else {
            results.remove(0)
        };

        match result {
            Ok(res) => {
                let mut output =
                    StructuredToolOutput::new(&self.name, true, res.clone(), Some(0), None, false);
                if self.name == "finish_task" {
                    output = output.with_finish_task_summary(res);
                }
                Ok(output.to_json_string().unwrap())
            }
            Err(err) => Err(ToolError::ExecutionFailed(err)),
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
        "A tool that blocks until cancelled".to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "OBJECT",
            "properties": {}
        })
    }

    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok(
            StructuredToolOutput::new(&self.name, true, "done".to_string(), Some(0), None, false)
                .to_json_string()
                .unwrap(),
        )
    }
}
