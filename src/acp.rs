#[cfg(feature = "acp")]
use crate::core::AgentOutput;
#[cfg(feature = "acp")]
use crate::session_manager::SessionManager;
#[cfg(feature = "acp")]
use axum::{
    extract::State,
    response::Html,
    routing::{get, post},
    Json, Router,
};
#[cfg(feature = "acp")]
use serde::{Deserialize, Serialize};
#[cfg(feature = "acp")]
use std::net::SocketAddr;
#[cfg(feature = "acp")]
use std::sync::Arc;

#[cfg(feature = "acp")]
#[derive(Debug, Serialize, Deserialize)]
pub struct AcpCapability {
    pub name: String,
    pub description: String,
    pub parameters_schema: serde_json::Value,
}

#[cfg(feature = "acp")]
#[derive(Debug, Serialize, Deserialize)]
pub struct AcpCapabilitiesResponse {
    pub agent_id: String,
    pub capabilities: Vec<AcpCapability>,
}

#[cfg(feature = "acp")]
#[derive(Debug, Serialize, Deserialize)]
pub struct AcpRunRequest {
    pub task: String,
    pub session_id: Option<String>,
}

#[cfg(feature = "acp")]
#[derive(Debug, Serialize, Deserialize)]
pub struct AcpRunResponse {
    pub session_id: String,
    pub status: String,
    pub output: String,
}

#[cfg(feature = "acp")]
pub struct AcpServer {
    pub session_manager: Arc<SessionManager>,
}

#[cfg(feature = "acp")]
impl AcpServer {
    pub fn new(session_manager: Arc<SessionManager>) -> Self {
        Self { session_manager }
    }

    pub async fn run(
        self,
        addr: SocketAddr,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let app = Router::new()
            .route("/", get(handle_index))
            .route("/capabilities", get(handle_capabilities))
            .route("/run", post(handle_run))
            .with_state(Arc::new(self));

        tracing::info!("ACP Server listening on {}", addr);
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app).await?;
        Ok(())
    }
}

#[cfg(feature = "acp")]
async fn handle_index() -> Html<&'static str> {
    Html(
        r#"
<!DOCTYPE html>
<html lang="zh-CN">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>JaviRust ACP 控制面板</title>
    <style>
        :root {
            --primary: #00d2ff;
            --bg: #0f172a;
            --card: #1e293b;
            --text: #f8fafc;
            --accent: #38bdf8;
        }
        body {
            font-family: 'Segoe UI', Tahoma, Geneva, Verdana, sans-serif;
            background-color: var(--bg);
            color: var(--text);
            margin: 0;
            padding: 20px;
            display: flex;
            flex-direction: column;
            align-items: center;
        }
        .container {
            width: 100%;
            max-width: 900px;
        }
        header {
            text-align: center;
            margin-bottom: 40px;
        }
        h1 {
            color: var(--primary);
            font-size: 2.5em;
            margin-bottom: 10px;
            text-shadow: 0 0 10px rgba(0, 210, 255, 0.3);
        }
        .status-badge {
            background: rgba(56, 189, 248, 0.2);
            color: var(--accent);
            padding: 4px 12px;
            border-radius: 20px;
            font-size: 0.9em;
            border: 1px solid var(--accent);
        }
        .card {
            background: var(--card);
            border-radius: 12px;
            padding: 24px;
            margin-bottom: 24px;
            box-shadow: 0 4px 6px -1px rgba(0, 0, 0, 0.1);
            border: 1px solid rgba(255, 255, 255, 0.05);
        }
        .section-title {
            font-size: 1.2em;
            font-weight: bold;
            margin-bottom: 16px;
            display: flex;
            align-items: center;
            color: var(--accent);
        }
        .capability-list {
            display: grid;
            grid-template-columns: repeat(auto-fill, minmax(250px, 1fr));
            gap: 16px;
        }
        .capability-item {
            background: rgba(15, 23, 42, 0.5);
            padding: 12px;
            border-radius: 8px;
            border-left: 4px solid var(--primary);
        }
        .capability-name {
            font-weight: bold;
            display: block;
            margin-bottom: 4px;
        }
        .capability-desc {
            font-size: 0.85em;
            color: #94a3b8;
            margin-bottom: 8px;
        }
        .capability-schema {
            font-size: 0.8em;
            color: #64748b;
            background: rgba(0, 0, 0, 0.3);
            padding: 8px;
            border-radius: 4px;
            font-family: 'Courier New', Courier, monospace;
            white-space: pre-wrap;
            border: 1px solid rgba(56, 189, 248, 0.2);
            margin-top: 4px;
        }
        textarea {
            width: 100%;
            height: 120px;
            background: #0f172a;
            border: 1px solid #334155;
            border-radius: 8px;
            color: white;
            padding: 12px;
            font-size: 1em;
            resize: vertical;
            box-sizing: border-box;
            margin-bottom: 16px;
        }
        button {
            background: var(--primary);
            color: #0f172a;
            border: none;
            padding: 12px 24px;
            border-radius: 8px;
            font-weight: bold;
            cursor: pointer;
            transition: all 0.2s;
            font-size: 1em;
        }
        button:hover {
            transform: translateY(-2px);
            box-shadow: 0 4px 12px rgba(0, 210, 255, 0.4);
        }
        button:disabled {
            background: #475569;
            cursor: not-allowed;
            transform: none;
        }
        #output {
            background: #000;
            color: #10b981;
            padding: 16px;
            border-radius: 8px;
            font-family: 'Courier New', Courier, monospace;
            white-space: pre-wrap;
            min-height: 200px;
            max-height: 500px;
            overflow-y: auto;
            border: 1px solid #064e3b;
        }
        .loader {
            display: none;
            margin-left: 10px;
            vertical-align: middle;
        }
    </style>
</head>
<body>
    <div class="container">
        <header>
            <h1>JaviRust <span style="font-weight: 300">ACP Console</span></h1>
            <span class="status-badge">● Agent Communication Protocol Enabled</span>
        </header>

        <div class="card">
            <div class="section-title">🛠️ 代理能力 (Capabilities)</div>
            <div id="capability-list" class="capability-list">
                正在获取能力列表...
            </div>
        </div>

        <div class="card">
            <div class="section-title">🚀 执行任务 (Execute Task)</div>
            <textarea id="task-input" placeholder="输入您想让 JaviRust 执行的任务，例如：'帮我写一个 Rust 的 Hello World'"></textarea>
            <div style="display: flex; align-items: center;">
                <button id="run-btn">运行任务 (Run)</button>
                <div id="loader" class="loader">⚙️ 处理中...</div>
            </div>
        </div>

        <div class="card">
            <div class="section-title">📄 执行结果 (Output)</div>
            <div id="output">等待指令...</div>
        </div>
    </div>

    <script>
        async function fetchCapabilities() {
            try {
                const res = await fetch('/capabilities');
                const data = await res.json();
                const list = document.getElementById('capability-list');
                list.innerHTML = data.capabilities.map(cap => `
                    <div class="capability-item">
                        <span class="capability-name">${cap.name}</span>
                        <span class="capability-desc">${cap.description}</span>
                        <div class="capability-schema"><strong>Request Schema:</strong><br>${JSON.stringify(cap.parameters_schema, null, 2)}</div>
                    </div>
                `).join('');
            } catch (err) {
                document.getElementById('capability-list').innerText = '获取能力失败: ' + err;
            }
        }

        document.getElementById('run-btn').addEventListener('click', async () => {
            const task = document.getElementById('task-input').value.trim();
            if (!task) return;

            const btn = document.getElementById('run-btn');
            const loader = document.getElementById('loader');
            const output = document.getElementById('output');

            btn.disabled = true;
            loader.style.display = 'inline-block';
            output.innerText = '正在启动 Agent...\n';

            try {
                const res = await fetch('/run', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ task: task })
                });
                const data = await res.json();
                output.innerText = `[状态: ${data.status}]\n\n${data.output}`;
            } catch (err) {
                output.innerText = '执行出错: ' + err;
            } finally {
                btn.disabled = false;
                loader.style.display = 'none';
            }
        });

        fetchCapabilities();
    </script>
</body>
</html>
    "#,
    )
}

#[cfg(feature = "acp")]
async fn handle_capabilities(
    State(server): State<Arc<AcpServer>>,
) -> Json<AcpCapabilitiesResponse> {
    let mut caps = Vec::new();

    // Get tools from session manager
    for tool in server.session_manager.tools() {
        caps.push(AcpCapability {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            parameters_schema: tool.parameters_schema(),
        });
    }

    // Add generic task execution
    caps.push(AcpCapability {
        name: "execute_task".to_string(),
        description: "Execute a natural language task using available tools".to_string(),
        parameters_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "task": { "type": "string" }
            }
        }),
    });

    Json(AcpCapabilitiesResponse {
        agent_id: "rusty-claw-v1".to_string(),
        capabilities: caps,
    })
}

#[cfg(feature = "acp")]
struct AcpOutput {
    buffer: Arc<std::sync::Mutex<String>>,
}

#[cfg(feature = "acp")]
#[async_trait::async_trait]
impl AgentOutput for AcpOutput {
    async fn on_waiting(&self, _message: &str) {}
    fn clear_waiting(&self) {}
    async fn on_text(&self, text: &str) {
        let mut b = self.buffer.lock().unwrap();
        b.push_str(text);
    }
    async fn on_thinking(&self, _text: &str) {}
    async fn on_tool_start(&self, _name: &str, _args: &str) {}
    async fn on_tool_end(&self, _result: &str) {}
    async fn on_error(&self, error: &str) {
        let mut b = self.buffer.lock().unwrap();
        b.push_str(&format!("\n[Error] {}\n", error));
    }
}

#[cfg(feature = "acp")]
async fn handle_run(
    State(server): State<Arc<AcpServer>>,
    Json(req): Json<AcpRunRequest>,
) -> Json<AcpRunResponse> {
    let session_id = req
        .session_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let buffer = Arc::new(std::sync::Mutex::new(String::new()));
    let output = Arc::new(AcpOutput {
        buffer: buffer.clone(),
    });

    let agent_res = server
        .session_manager
        .get_or_create_session(&session_id, output)
        .await;

    match agent_res {
        Ok(agent_mutex) => {
            let mut agent = agent_mutex.lock().await;
            match agent.step(req.task).await {
                Ok(exit) => {
                    let final_output = buffer.lock().unwrap().clone();
                    Json(AcpRunResponse {
                        session_id,
                        status: format!("{:?}", exit),
                        output: final_output,
                    })
                }
                Err(e) => Json(AcpRunResponse {
                    session_id,
                    status: "Error".to_string(),
                    output: e.to_string(),
                }),
            }
        }
        Err(e) => Json(AcpRunResponse {
            session_id,
            status: "SessionCreationFailed".to_string(),
            output: e,
        }),
    }
}

#[cfg(not(feature = "acp"))]
pub struct AcpServer;
#[cfg(not(feature = "acp"))]
impl AcpServer {
    pub fn new(_: std::sync::Arc<crate::session_manager::SessionManager>) -> Self {
        Self
    }
    pub async fn run(
        self,
        _: std::net::SocketAddr,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Err("ACP feature not enabled".into())
    }
}
