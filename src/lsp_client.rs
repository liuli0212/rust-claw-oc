use lsp_types::{
    request::{DocumentSymbolRequest, GotoDefinition, HoverRequest, Initialize, References},
    ClientCapabilities, DocumentSymbolResponse, GotoDefinitionParams, Hover, HoverParams,
    InitializeParams, InitializeResult, Position, PublishDiagnosticsParams, ReferenceParams,
    TextDocumentIdentifier, TextDocumentPositionParams, Url,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, Mutex};

// JSON-RPC message structures are now handled via serde_json::Value for more robust matching.

pub struct LazyLspClient {
    root_dir: PathBuf,
    client: Mutex<Option<Arc<LspClient>>>,
}

impl LazyLspClient {
    pub fn new(root_dir: PathBuf) -> Self {
        Self {
            root_dir,
            client: Mutex::new(None),
        }
    }

    pub async fn get_client(&self) -> Result<Arc<LspClient>, String> {
        let mut guard = self.client.lock().await;
        if let Some(client) = guard.as_ref() {
            return Ok(client.clone());
        }

        tracing::info!("Lazy initializing LSP client (rust-analyzer)...");
        let client = LspClient::start(self.root_dir.clone()).await?;
        *guard = Some(client.clone());
        Ok(client)
    }
}

pub struct LspClient {
    _child: Mutex<Child>,
    writer: Mutex<tokio::process::ChildStdin>,
    next_id: AtomicU64,
    pending_requests: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    diagnostics: Mutex<HashMap<Url, Vec<lsp_types::Diagnostic>>>,
}

impl LspClient {
    pub async fn start(root_dir: PathBuf) -> Result<Arc<Self>, String> {
        let ra_path = dirs::home_dir()
            .map(|h| h.join(".local/bin/rust-analyzer"))
            .filter(|p| p.exists())
            .unwrap_or_else(|| PathBuf::from("rust-analyzer"));

        let mut child = Command::new(ra_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to start rust-analyzer: {}", e))?;

        let stdin = child.stdin.take().ok_or("Failed to open stdin")?;
        let stdout = child.stdout.take().ok_or("Failed to open stdout")?;
        let stderr = child.stderr.take().ok_or("Failed to open stderr")?;

        let client = Arc::new(Self {
            _child: Mutex::new(child),
            writer: Mutex::new(stdin),
            next_id: AtomicU64::new(1),
            pending_requests: Mutex::new(HashMap::new()),
            diagnostics: Mutex::new(HashMap::new()),
        });

        // Spawn stderr consumer
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            while let Ok(n) = reader.read_line(&mut line).await {
                if n == 0 {
                    break;
                }
                tracing::debug!("LSP stderr: {}", line.trim());
                line.clear();
            }
        });

        let client_clone = client.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                if let Err(e) = reader.read_line(&mut line).await {
                    tracing::error!("LSP reader error: {}", e);
                    break;
                }
                if line.is_empty() {
                    break;
                }

                if line.starts_with("Content-Length: ") {
                    let len_res: Result<usize, _> = line["Content-Length: ".len()..].trim().parse();

                    let len = match len_res {
                        Ok(l) => l,
                        Err(_) => {
                            tracing::error!("Invalid Content-Length header: {}", line);
                            continue;
                        }
                    };

                    // Read the empty line after Content-Length
                    let mut empty_line = String::new();
                    if reader.read_line(&mut empty_line).await.is_err() {
                        break;
                    }

                    let mut body = vec![0u8; len];
                    if let Err(e) = reader.read_exact(&mut body).await {
                        tracing::error!("LSP reader error reading body: {}", e);
                        break;
                    }

                    let body_str = String::from_utf8_lossy(&body);
                    let val: Value = match serde_json::from_str(&body_str) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::error!(
                                "Failed to parse LSP message: {} | Body: {}",
                                e,
                                body_str
                            );
                            continue;
                        }
                    };

                    if let Some(id) = val.get("id").and_then(|i| i.as_u64()) {
                        // It's a response
                        let mut pending = client_clone.pending_requests.lock().await;
                        if let Some(tx) = pending.remove(&id) {
                            if let Some(error) = val.get("error") {
                                let _ = tx.send(Err(error.to_string()));
                            } else {
                                let _ =
                                    tx.send(Ok(val.get("result").cloned().unwrap_or(Value::Null)));
                            }
                        }
                    } else if let Some(method) = val.get("method").and_then(|m| m.as_str()) {
                        // It's a notification
                        if method == "textDocument/publishDiagnostics" {
                            if let Some(params) = val.get("params") {
                                if let Ok(diagnostics) =
                                    serde_json::from_value::<PublishDiagnosticsParams>(
                                        params.clone(),
                                    )
                                {
                                    let mut diags = client_clone.diagnostics.lock().await;
                                    diags.insert(diagnostics.uri, diagnostics.diagnostics);
                                }
                            }
                        }
                    }
                }
            }
        });

        // Initialize
        let root_uri = Url::from_directory_path(&root_dir).map_err(|_| "Invalid root path")?;
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            capabilities: ClientCapabilities {
                text_document: Some(lsp_types::TextDocumentClientCapabilities {
                    document_symbol: Some(lsp_types::DocumentSymbolClientCapabilities {
                        hierarchical_document_symbol_support: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            workspace_folders: Some(vec![lsp_types::WorkspaceFolder {
                uri: root_uri,
                name: "workspace".to_string(),
            }]),
            ..Default::default()
        };

        let _init_result: InitializeResult = client
            .request::<Initialize>(params)
            .await
            .map_err(|e| format!("LSP initialization failed: {}", e))?;

        // Notify initialized
        client.notify("initialized", json!({})).await?;

        Ok(client)
    }

    pub async fn request<R>(&self, params: R::Params) -> Result<R::Result, String>
    where
        R: lsp_types::request::Request,
        R::Params: Serialize,
        R::Result: for<'de> Deserialize<'de>,
    {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": R::METHOD,
            "params": params,
        });

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending_requests.lock().await;
            pending.insert(id, tx);
        }

        let body = serde_json::to_string(&req).unwrap();
        let msg = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);

        {
            let mut writer = self.writer.lock().await;
            writer
                .write_all(msg.as_bytes())
                .await
                .map_err(|e| format!("Failed to write to LSP: {}", e))?;
            writer
                .flush()
                .await
                .map_err(|e| format!("Failed to flush LSP: {}", e))?;
        }

        let res_val = rx
            .await
            .map_err(|_| "LSP request cancelled".to_string())??;
        serde_json::from_value(res_val).map_err(|e| format!("Failed to parse LSP response: {}", e))
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let notif = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });

        let body = serde_json::to_string(&notif).unwrap();
        let msg = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);

        let mut writer = self.writer.lock().await;
        writer
            .write_all(msg.as_bytes())
            .await
            .map_err(|e| format!("Failed to write to LSP: {}", e))?;
        writer
            .flush()
            .await
            .map_err(|e| format!("Failed to flush LSP: {}", e))?;
        Ok(())
    }

    fn get_uri(&self, path: &PathBuf) -> Result<Url, String> {
        let abs_path = path
            .canonicalize()
            .map_err(|e| format!("Failed to canonicalize path {:?}: {}", path, e))?;
        Url::from_file_path(&abs_path).map_err(|_| format!("Invalid file path: {:?}", abs_path))
    }

    pub async fn goto_definition(
        &self,
        path: PathBuf,
        line: u32,
        character: u32,
    ) -> Result<Option<lsp_types::GotoDefinitionResponse>, String> {
        let uri = self.get_uri(&path)?;
        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position { line, character },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        self.request::<GotoDefinition>(params).await
    }

    pub async fn find_references(
        &self,
        path: PathBuf,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<Option<Vec<lsp_types::Location>>, String> {
        let uri = self.get_uri(&path)?;
        let params = ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position { line, character },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: lsp_types::ReferenceContext {
                include_declaration,
            },
        };

        self.request::<References>(params).await
    }

    pub async fn hover(
        &self,
        path: PathBuf,
        line: u32,
        character: u32,
    ) -> Result<Option<Hover>, String> {
        let uri = self.get_uri(&path)?;
        let params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position { line, character },
            },
            work_done_progress_params: Default::default(),
        };

        self.request::<HoverRequest>(params).await
    }

    pub async fn document_symbols(
        &self,
        path: PathBuf,
    ) -> Result<Option<DocumentSymbolResponse>, String> {
        let uri = self.get_uri(&path)?;
        let params = lsp_types::DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        self.request::<DocumentSymbolRequest>(params).await
    }

    pub async fn get_diagnostics(
        &self,
        path: PathBuf,
    ) -> Result<Vec<lsp_types::Diagnostic>, String> {
        let uri = self.get_uri(&path)?;
        let diags = self.diagnostics.lock().await;
        Ok(diags.get(&uri).cloned().unwrap_or_default())
    }
}
