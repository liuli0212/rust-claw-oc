#[cfg(feature = "acp")]
use super::{
    output::{AcpEvent, AcpOutput, CancelGuard},
    AcpCapabilitiesResponse, AcpCapability, AcpRunRequest, AcpServer,
};
#[cfg(feature = "acp")]
use axum::{
    extract::State,
    response::sse::{Event, Sse},
    Json,
};
#[cfg(feature = "acp")]
use std::sync::Arc;
#[cfg(feature = "acp")]
use tokio::sync::mpsc;
#[cfg(feature = "acp")]
use tokio_stream::StreamExt;

#[cfg(feature = "acp")]
pub(super) async fn handle_capabilities(
    State(server): State<Arc<AcpServer>>,
) -> Json<AcpCapabilitiesResponse> {
    let mut caps = Vec::new();

    for tool in server.session_manager.tools() {
        caps.push(AcpCapability {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            parameters_schema: tool.parameters_schema(),
        });
    }

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
pub(super) async fn handle_run(
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
        .get_or_create_session(&session_id, &session_id, output.clone())
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
