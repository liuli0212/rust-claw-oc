use std::time::SystemTime;
use tokio::sync::mpsc;
use tracing::{info, span, Level};

use crate::schema::CorrelationIds;

/// Emits telemetry events asynchronously so writing tracing spans/metrics
/// won't block the main agent/tool execution threads.
pub struct TelemetryExporter {
    sender: mpsc::Sender<TelemetryMessage>,
}

#[derive(Debug)]
enum TelemetryMessage {
    SpanStart(String, CorrelationIds, u64),
    SpanEnd(String, u64),
}

impl TelemetryExporter {
    pub fn new() -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = mpsc::channel(1000); // Backpressure is buffered

        let handle = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                match msg {
                    TelemetryMessage::SpanStart(name, ids, ts) => {
                        let _span = span!(
                            Level::INFO,
                            "AgentSpan",
                            span_name = %name,
                            session_id = %ids.session_id,
                            task_id = ?ids.task_id,
                            run_id = ?ids.run_id,
                            turn_id = ?ids.turn_id,
                            event_id = ?ids.event_id,
                            start_ts = %ts
                        );
                        _span.in_scope(|| {
                            info!("SpanStarted: {}", name);
                        });
                    }
                    TelemetryMessage::SpanEnd(name, ts) => {
                        info!("SpanEnded: {} at {}", name, ts);
                    }
                }
            }
        });

        (Self { sender: tx }, handle)
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    pub fn start_span(&self, name: &str, ids: CorrelationIds) {
        let _ = self.sender.try_send(TelemetryMessage::SpanStart(
            name.to_string(),
            ids,
            Self::now(),
        ));
    }

    pub fn end_span(&self, name: &str) {
        let _ = self
            .sender
            .try_send(TelemetryMessage::SpanEnd(name.to_string(), Self::now()));
    }
}
