#[cfg(feature = "acp")]
use super::{
    output::{AcpEvent, AcpOutput, CancelGuard},
    AcpCapabilitiesResponse, AcpCapability, AcpRunRequest, AcpServer,
};
#[cfg(feature = "acp")]
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event, Sse},
    Json,
};
#[cfg(feature = "acp")]
use serde::Deserialize;
#[cfg(feature = "acp")]
use std::sync::Arc;
#[cfg(feature = "acp")]
use tokio::sync::mpsc;
#[cfg(feature = "acp")]
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

#[cfg(feature = "acp")]
#[derive(Debug, Deserialize)]
pub(super) struct TraceRunsQuery {
    session_id: Option<String>,
    task_id: Option<String>,
    status: Option<String>,
    tool_name: Option<String>,
    query: Option<String>,
    from: Option<u64>,
    to: Option<u64>,
    min_duration_ms: Option<u64>,
}

#[cfg(feature = "acp")]
#[derive(Debug, Deserialize)]
pub(super) struct TraceRecordsQuery {
    actor: Option<String>,
    name: Option<String>,
    status: Option<String>,
    turn_id: Option<String>,
    iteration: Option<u32>,
}

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
        .unwrap_or_else(|| format!("acp_{}", uuid::Uuid::new_v4().simple()));

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

#[cfg(feature = "acp")]
pub(super) async fn handle_trace_runs(
    Query(query): Query<TraceRunsQuery>,
) -> Json<Vec<crate::trace::RunSummary>> {
    Json(crate::trace::list_runs(&crate::trace::RunQuery {
        session_id: query.session_id,
        task_id: query.task_id,
        status: query.status,
        tool_name: query.tool_name,
        query: query.query,
        from_unix_ms: query.from,
        to_unix_ms: query.to,
        min_duration_ms: query.min_duration_ms,
    }))
}

#[cfg(feature = "acp")]
pub(super) async fn handle_trace_run(
    Path(run_id): Path<String>,
) -> Result<Json<crate::trace::RunOverview>, StatusCode> {
    crate::trace::get_run_overview(&run_id)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

#[cfg(feature = "acp")]
pub(super) async fn handle_trace_records(
    Path(run_id): Path<String>,
    Query(query): Query<TraceRecordsQuery>,
) -> Json<Vec<crate::trace::TraceRecord>> {
    Json(crate::trace::get_records(
        &run_id,
        &crate::trace::RecordQuery {
            actor: query.actor,
            name: query.name,
            status: query.status,
            turn_id: query.turn_id,
            iteration: query.iteration,
        },
    ))
}

#[cfg(feature = "acp")]
pub(super) async fn handle_trace_tree(
    Path(run_id): Path<String>,
) -> Json<Vec<crate::trace::TraceTreeNode>> {
    Json(crate::trace::get_tree(&run_id))
}

#[cfg(feature = "acp")]
pub(super) async fn handle_trace_artifacts(
    Path(run_id): Path<String>,
) -> Json<crate::trace::TraceArtifacts> {
    Json(crate::trace::get_artifacts(&run_id))
}

#[cfg(feature = "acp")]
pub(super) async fn handle_live_trace(
    Path(session_id): Path<String>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let stream =
        BroadcastStream::new(crate::trace::shared_bus().subscribe()).filter_map(move |result| {
            let Ok(record) = result else {
                return None;
            };
            let matches_session = record.session_id == session_id
                || record
                    .attrs
                    .get("root_session_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(session_id.as_str());
            if !matches_session {
                return None;
            }
            Some(Ok(
                Event::default().data(serde_json::to_string(&record).unwrap())
            ))
        });

    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}
