#[cfg(feature = "acp")]
use crate::core::AgentOutput;
#[cfg(feature = "acp")]
use crate::session_manager::SessionManager;
#[cfg(feature = "acp")]
use axum::{
    extract::State,
    response::{
        sse::{Event, Sse},
        Html,
    },
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
use tokio::sync::mpsc;
#[cfg(feature = "acp")]
use tokio_stream::StreamExt;

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
#[derive(Debug, Serialize)]
#[serde(tag = "type", content = "data")]
enum AcpEvent {
    Text(String),
    Thinking(String),
    Error(String),
    Finish { summary: String, status: String },
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
            max-height: 600px;
            overflow-y: auto;
            border: 1px solid #064e3b;
            position: relative;
        }
        .output-controls {
            display: flex;
            justify-content: flex-end;
            margin-bottom: 8px;
        }
        .btn-small {
            padding: 4px 10px;
            font-size: 0.8em;
            background: #334155;
            color: #f8fafc;
            border: 1px solid #475569;
        }
        .btn-small:hover {
            background: #475569;
        }
        .loader {
            display: none;
            margin-left: 15px;
            vertical-align: middle;
            color: var(--accent);
            font-size: 0.9em;
            animation: pulse 1.5s infinite;
        }
        @keyframes pulse {
            0% { opacity: 1; }
            50% { opacity: 0.5; }
            100% { opacity: 1; }
        }
        .thinking { color: #94a3b8; font-style: italic; border-left: 2px solid #334155; padding-left: 10px; margin: 5px 0; }
        .tool { color: #38bdf8; font-weight: bold; }
        .error { color: #ef4444; background: rgba(239, 68, 68, 0.1); padding: 8px; border-radius: 4px; margin: 10px 0; }
        .finish { color: #00d2ff; background: rgba(0, 210, 255, 0.1); border: 1px solid rgba(0, 210, 255, 0.2); margin-top: 15px; padding: 12px; border-radius: 8px; }
    </style>
</head>
<body>
    <div class="container">
        <header {
            text-align: center;
            margin-bottom: 40px;
        }>
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
            <div class="output-controls">
                <button id="clear-btn" class="btn-small">清空输出 (Clear)</button>
            </div>
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

        document.getElementById('clear-btn').addEventListener('click', () => {
            document.getElementById('output').innerHTML = '输出已清空。<br>';
        });

        document.getElementById('run-btn').addEventListener('click', async () => {
            const task = document.getElementById('task-input').value.trim();
            if (!task) return;

            const btn = document.getElementById('run-btn');
            const loader = document.getElementById('loader');
            const output = document.getElementById('output');

            btn.disabled = true;
            loader.style.display = 'inline-block';
            if (output.innerText === '等待指令...' || output.innerText === '输出已清空。') {
                output.innerHTML = '正在启动 Agent...<br>';
            } else {
                output.innerHTML += '<hr style="border:0;border-top:1px solid #334155;margin:20px 0">';
            }

            try {
                const response = await fetch('/run', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ task: task })
                });

                if (!response.ok) {
                    throw new Error(`HTTP error! status: ${response.status}`);
                }

                const reader = response.body.getReader();
                const decoder = new TextDecoder();

                let buffer = '';
                while (true) {
                    const { value, done } = await reader.read();
                    if (done) break;
                    
                    buffer += decoder.decode(value, { stream: true });
                    const lines = buffer.split('\n');
                    buffer = lines.pop();

                    for (const line of lines) {
                        if (line.startsWith('data: ')) {
                            try {
                                const event = JSON.parse(line.substring(6));
                                // 收到 Finish 或 Error 立即停止转圈，不等待连接完全关闭
                                if (event.type === 'Finish' || event.type === 'Error') {
                                    btn.disabled = false;
                                    loader.style.display = 'none';
                                }
                                handleEvent(event, output);
                            } catch (e) {
                                console.error("Error parsing event", e, line);
                            }
                        }
                    }
                }
            } catch (err) {
                output.innerHTML += `<div class="error">执行出错: ${err}</div>`;
                btn.disabled = false;
                loader.style.display = 'none';
            }
        });

        const MAX_LOG_SIZE = 50000; // 50KB 缓冲区限制
        function handleEvent(event, output) {
            const div = document.createElement('div');
            
            // 鲁棒性：限制输出区域大小，防止内存溢出
            if (output.innerHTML.length > MAX_LOG_SIZE) {
                output.innerHTML = '... (由于内容过长，较旧的日志已自动清除) ...<br>' + 
                                 output.innerHTML.substring(MAX_LOG_SIZE / 2);
            }

            switch (event.type) {
                case 'Text':
                    output.appendChild(document.createTextNode(event.data));
                    break;
                case 'Thinking':
                    div.className = 'thinking';
                    div.innerText = event.data;
                    output.appendChild(div);
                    break;
                case 'Error':
                    div.className = 'error';
                    div.innerText = `❌ [Error] ${event.data}`;
                    output.appendChild(div);
                    break;
                case 'Finish':
                    div.className = 'finish';
                    div.innerHTML = `<strong>[任务状态: ${event.data.status}]</strong><br>${event.data.summary}`;
                    output.appendChild(div);
                    break;
            }
            
            // 平滑滚动
            output.scrollTo({
                top: output.scrollHeight,
                behavior: 'smooth'
            });
        }

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
    tx: mpsc::UnboundedSender<AcpEvent>,
}

#[cfg(feature = "acp")]
#[async_trait::async_trait]
impl AgentOutput for AcpOutput {
    async fn on_waiting(&self, _message: &str) {}
    fn clear_waiting(&self) {}
    async fn on_text(&self, text: &str) {
        let _ = self.tx.send(AcpEvent::Text(text.to_string()));
    }
    async fn on_thinking(&self, text: &str) {
        let _ = self.tx.send(AcpEvent::Thinking(text.to_string()));
    }
    async fn on_tool_start(&self, _name: &str, _args: &str) {}
    async fn on_tool_end(&self, _result: &str) {}
    async fn on_error(&self, error: &str) {
        let _ = self.tx.send(AcpEvent::Error(error.to_string()));
    }
    async fn on_task_finish(&self, summary: &str) {
        let _ = self.tx.send(AcpEvent::Finish {
            summary: summary.to_string(),
            status: "finished".to_string(),
        });
    }
}

#[cfg(feature = "acp")]
struct CancelGuard {
    agent: Arc<tokio::sync::Mutex<crate::core::AgentLoop>>,
}

#[cfg(feature = "acp")]
impl Drop for CancelGuard {
    fn drop(&mut self) {
        let agent = self.agent.clone();
        tokio::spawn(async move {
            let agent = agent.lock().await;
            agent.request_cancel();
            tracing::info!("ACP client disconnected, requested agent cancellation");
        });
    }
}

#[cfg(feature = "acp")]
async fn handle_run(
    State(server): State<Arc<AcpServer>>,
    Json(req): Json<AcpRunRequest>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let session_id = req
        .session_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let (tx, rx) = mpsc::unbounded_channel();
    let output = Arc::new(AcpOutput { tx: tx.clone() });

    let agent_res = server
        .session_manager
        .get_or_create_session(&session_id, output.clone())
        .await;

    let _guard = match agent_res {
        Ok(agent_mutex) => {
            let agent_mutex_for_run = agent_mutex.clone();
            let tx_for_run = tx.clone();
            let task = req.task.clone();

            tokio::spawn(async move {
                let mut agent = agent_mutex_for_run.lock().await;
                agent.flush_output().await;
                agent.update_output(output);
                match agent.step(task).await {
                    Ok(exit) => {
                        let _ = tx_for_run.send(AcpEvent::Finish {
                            summary: match &exit {
                                crate::core::RunExit::Finished(s) => s.clone(),
                                crate::core::RunExit::YieldedToUser => {
                                    "Agent is waiting for your input.".to_string()
                                }
                                _ => exit.label().to_string(),
                            },
                            status: exit.label().to_string(),
                        });
                    }
                    Err(e) => {
                        let _ = tx_for_run.send(AcpEvent::Error(e.to_string()));
                    }
                }
            });

            Some(CancelGuard { agent: agent_mutex })
        }
        Err(e) => {
            let _ = tx.send(AcpEvent::Error(format!("Session creation failed: {}", e)));
            None
        }
    };

    let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx).map(move |event| {
        let _ = &_guard;
        Ok::<_, std::convert::Infallible>(
            Event::default().data(serde_json::to_string(&event).unwrap()),
        )
    });

    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
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
