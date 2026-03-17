#[cfg(feature = "acp")]
use crate::session_manager::SessionManager;
#[cfg(feature = "acp")]
use axum::{
    response::Html,
    routing::{get, post},
    Router,
};
#[cfg(feature = "acp")]
use serde::{Deserialize, Serialize};
#[cfg(feature = "acp")]
use std::net::SocketAddr;
#[cfg(feature = "acp")]
use std::sync::Arc;

#[cfg(feature = "acp")]
mod handlers;
#[cfg(feature = "acp")]
mod output;

#[cfg(feature = "acp")]
use handlers::{handle_capabilities, handle_run};

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
    Html(include_str!("index.html"))
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
