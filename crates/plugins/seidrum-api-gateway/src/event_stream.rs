//! Real-time event stream over WebSocket.
//! Subscribes to all NATS events and forwards them to connected WebSocket clients
//! with optional subject pattern filtering.

pub mod trace_collector {
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Serialize};

    /// A single event within a trace.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct TraceSpan {
        pub event_id: String,
        pub subject: String,
        pub source: String,
        pub event_type: String,
        pub timestamp: DateTime<Utc>,
        /// Time in ms since the trace started
        pub offset_ms: i64,
        /// Payload preview (first 500 chars of serialized payload)
        pub payload_preview: String,
    }

    /// A complete trace: all events sharing a correlation_id.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct Trace {
        pub correlation_id: String,
        pub started_at: DateTime<Utc>,
        pub last_event_at: DateTime<Utc>,
        pub duration_ms: i64,
        pub span_count: usize,
        pub spans: Vec<TraceSpan>,
    }

    /// Request to get a specific trace.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct TraceGetRequest {
        pub correlation_id: String,
    }

    /// Request to list recent traces.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct TraceListRequest {
        pub limit: Option<usize>,
        pub since: Option<DateTime<Utc>>,
    }

    /// Response with trace list (summaries without full spans).
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct TraceListResponse {
        pub traces: Vec<TraceSummary>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct TraceSummary {
        pub correlation_id: String,
        pub started_at: DateTime<Utc>,
        pub duration_ms: i64,
        pub span_count: usize,
        /// First event subject (usually the trigger)
        pub trigger_subject: String,
        /// Last event subject
        pub last_subject: String,
    }
}

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use seidrum_common::nats_utils::NatsClient;
use serde::Serialize;
use tracing::{debug, error, info};

#[derive(serde::Deserialize)]
pub struct EventStreamQuery {
    /// Optional subject filter pattern (e.g., "channel.*", "llm.*")
    pub filter: Option<String>,
    /// Optional correlation_id to follow a specific trace
    pub correlation_id: Option<String>,
    /// API key for authentication (required for WebSocket upgrade)
    pub api_key: Option<String>,
}

#[derive(Serialize)]
pub struct StreamEvent {
    pub subject: String,
    pub timestamp: String,
    pub source: Option<String>,
    pub correlation_id: Option<String>,
    pub event_type: Option<String>,
    pub payload_preview: String,
}

pub async fn handle_event_stream(
    socket: WebSocket,
    nats: NatsClient,
    filter: Option<String>,
    correlation_id_filter: Option<String>,
) {
    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Subscribe to all events or a filtered pattern
    let subject_pattern = filter.as_deref().unwrap_or(">");
    let mut sub = match nats.inner().subscribe(subject_pattern.to_string()).await {
        Ok(s) => s,
        Err(e) => {
            error!(error = %e, "Failed to subscribe to NATS for event stream");
            let _ = ws_sender
                .send(Message::Text(
                    serde_json::json!({"error": "Failed to subscribe to events"})
                        .to_string()
                        .into(),
                ))
                .await;
            return;
        }
    };

    info!(
        pattern = %subject_pattern,
        correlation_id = ?correlation_id_filter,
        "Event stream WebSocket connected"
    );

    // Spawn a task to detect client disconnect
    let (close_tx, mut close_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_receiver.next().await {
            if matches!(msg, Message::Close(_)) {
                break;
            }
        }
        let _ = close_tx.send(());
    });

    loop {
        tokio::select! {
            Some(msg) = sub.next() => {
                let subject = msg.subject.to_string();

                // Skip internal subjects
                if subject.starts_with("_INBOX") || subject.starts_with("trace.") {
                    continue;
                }

                // Parse envelope for metadata
                let envelope: serde_json::Value = serde_json::from_slice(&msg.payload)
                    .unwrap_or(serde_json::json!(null));

                // Apply correlation_id filter if set
                if let Some(ref cid_filter) = correlation_id_filter {
                    let event_cid = envelope.get("correlation_id").and_then(|v| v.as_str());
                    if event_cid != Some(cid_filter.as_str()) {
                        continue;
                    }
                }

                let stream_event = StreamEvent {
                    subject,
                    timestamp: envelope
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    source: envelope
                        .get("source")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    correlation_id: envelope
                        .get("correlation_id")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    event_type: envelope
                        .get("event_type")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    payload_preview: envelope
                        .get("payload")
                        .map(|p| {
                            let s = p.to_string();
                            if s.len() > 1000 {
                                format!("{}...", &s[..1000])
                            } else {
                                s
                            }
                        })
                        .unwrap_or_default(),
                };

                if let Ok(json) = serde_json::to_string(&stream_event) {
                    if ws_sender.send(Message::Text(json.into())).await.is_err() {
                        debug!("Event stream client disconnected");
                        break;
                    }
                }
            }
            _ = &mut close_rx => {
                debug!("Event stream client sent close");
                break;
            }
        }
    }

    info!("Event stream WebSocket disconnected");
}
