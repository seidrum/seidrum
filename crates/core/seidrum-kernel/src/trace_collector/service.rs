use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{info, warn};

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
    /// Authenticated source identifier (required for access control).
    #[serde(default)]
    pub auth_source: Option<String>,
}

/// Request to list recent traces.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TraceListRequest {
    pub limit: Option<usize>,
    pub since: Option<DateTime<Utc>>,
    /// Authenticated source identifier (required for access control).
    #[serde(default)]
    pub auth_source: Option<String>,
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

/// Trusted sources allowed to query traces.
const TRUSTED_SOURCES: &[&str] = &["api-gateway", "kernel", "seidrum-cli", "dashboard"];

/// Validate that a trace query comes from a trusted source.
fn is_authorized(auth_source: &Option<String>) -> bool {
    match auth_source {
        Some(source) => TRUSTED_SOURCES.iter().any(|&s| s == source),
        None => false,
    }
}

pub struct TraceCollectorService {
    nats: seidrum_common::bus_client::BusClient,
    /// correlation_id -> Trace
    traces: Arc<RwLock<HashMap<String, Trace>>>,
    max_traces: usize,
    /// Maximum number of spans per trace to prevent unbounded growth
    max_spans_per_trace: usize,
}

impl TraceCollectorService {
    pub fn new(nats: seidrum_common::bus_client::BusClient, max_traces: usize) -> Self {
        Self {
            nats,
            traces: Arc::new(RwLock::new(HashMap::with_capacity(max_traces))),
            max_traces,
            max_spans_per_trace: 500,
        }
    }

    pub async fn spawn(self) -> Result<tokio::task::JoinHandle<()>> {
        // Subscribe to all subjects
        let mut all_events = self
            .nats
            .subscribe(">".to_string())
            .await
            .context("failed to subscribe to all subjects")?;

        // Subscribe to trace query subjects
        let mut trace_get_sub = self
            .nats
            .subscribe("trace.get".to_string())
            .await
            .context("failed to subscribe to trace.get")?;
        let mut trace_list_sub = self
            .nats
            .subscribe("trace.list".to_string())
            .await
            .context("failed to subscribe to trace.list")?;

        let traces = self.traces.clone();
        let nats = self.nats.clone();
        let max_traces = self.max_traces;
        let max_spans_per_trace = self.max_spans_per_trace;

        // Spawn periodic compaction task to evict stale traces
        let compaction_traces = self.traces.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                let mut store = compaction_traces.write().await;
                let cutoff = Utc::now() - chrono::Duration::minutes(30);
                let before = store.len();
                store.retain(|_, t| t.last_event_at > cutoff);
                let evicted = before - store.len();
                if evicted > 0 {
                    info!(
                        evicted,
                        remaining = store.len(),
                        "Trace compaction: evicted stale traces"
                    );
                }
            }
        });

        let handle = tokio::spawn(async move {
            info!(max_traces, "Trace collector service started");

            loop {
                tokio::select! {
                    Some(msg) = all_events.next() => {
                        // Skip trace.* subjects to avoid recursion
                        let subject = msg.subject.to_string();
                        if subject.starts_with("trace.") || subject.starts_with("_INBOX") {
                            continue;
                        }

                        // Try to parse as EventEnvelope
                        if let Ok(envelope) = serde_json::from_slice::<serde_json::Value>(&msg.payload) {
                            if let Some(correlation_id) = envelope.get("correlation_id").and_then(|v| v.as_str()) {
                                let event_id = envelope.get("id").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
                                let source = envelope.get("source").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
                                let event_type = envelope.get("event_type").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
                                let timestamp = envelope.get("timestamp")
                                    .and_then(|v| v.as_str())
                                    .and_then(|s| s.parse::<DateTime<Utc>>().ok())
                                    .unwrap_or_else(Utc::now);

                                let payload_preview = envelope.get("payload")
                                    .map(|p| {
                                        let s = p.to_string();
                                        if s.len() > 500 { format!("{}...", &s[..500]) } else { s }
                                    })
                                    .unwrap_or_default();

                                let mut store = traces.write().await;

                                let trace = store.entry(correlation_id.to_string()).or_insert_with(|| {
                                    Trace {
                                        correlation_id: correlation_id.to_string(),
                                        started_at: timestamp,
                                        last_event_at: timestamp,
                                        duration_ms: 0,
                                        span_count: 0,
                                        spans: Vec::new(),
                                    }
                                });

                                let offset_ms = (timestamp - trace.started_at).num_milliseconds();

                                // Cap spans per trace to prevent unbounded memory growth
                                if trace.spans.len() >= max_spans_per_trace {
                                    // Update timing but don't add more spans
                                    trace.last_event_at = timestamp;
                                    trace.duration_ms = (timestamp - trace.started_at).num_milliseconds();
                                    trace.span_count += 1; // Track real count even if spans are dropped
                                    continue;
                                }

                                trace.spans.push(TraceSpan {
                                    event_id,
                                    subject: subject.clone(),
                                    source,
                                    event_type,
                                    timestamp,
                                    offset_ms,
                                    payload_preview,
                                });

                                trace.last_event_at = timestamp;
                                trace.duration_ms = (timestamp - trace.started_at).num_milliseconds();
                                trace.span_count = trace.spans.len();

                                // Evict oldest traces if over limit
                                if store.len() > max_traces {
                                    // Find the oldest trace
                                    if let Some(oldest_id) = store
                                        .iter()
                                        .min_by_key(|(_, t)| t.last_event_at)
                                        .map(|(id, _)| id.clone())
                                    {
                                        store.remove(&oldest_id);
                                    }
                                }
                            }
                        }
                    }

                    Some(msg) = trace_get_sub.next() => {
                        let reply = match &msg.reply {
                            Some(r) => r.clone(),
                            None => continue,
                        };

                        if let Ok(req) = serde_json::from_slice::<TraceGetRequest>(&msg.payload) {
                            // Authenticate the request
                            if !is_authorized(&req.auth_source) {
                                warn!(
                                    auth_source = ?req.auth_source,
                                    "Unauthorized trace.get request rejected"
                                );
                                let error = serde_json::json!({"error": "unauthorized"});
                                if let Ok(bytes) = serde_json::to_vec(&error) {
                                    let _ = nats.publish_bytes(reply, bytes).await;
                                }
                                continue;
                            }

                            let store = traces.read().await;
                            let response = store.get(&req.correlation_id).cloned();
                            if let Ok(bytes) = serde_json::to_vec(&response) {
                                let _ = nats.publish_bytes(reply.clone(), bytes).await;
                            }
                        }
                    }

                    Some(msg) = trace_list_sub.next() => {
                        let reply = match &msg.reply {
                            Some(r) => r.clone(),
                            None => continue,
                        };

                        let req: TraceListRequest = serde_json::from_slice(&msg.payload)
                            .unwrap_or(TraceListRequest { limit: Some(50), since: None, auth_source: None });

                        // Authenticate the request
                        if !is_authorized(&req.auth_source) {
                            warn!(
                                auth_source = ?req.auth_source,
                                "Unauthorized trace.list request rejected"
                            );
                            let error_resp = TraceListResponse { traces: vec![] };
                            if let Ok(bytes) = serde_json::to_vec(&error_resp) {
                                let _ = nats.publish_bytes(reply, bytes).await;
                            }
                            continue;
                        }

                        let store = traces.read().await;
                        let limit = req.limit.unwrap_or(50);

                        let mut summaries: Vec<TraceSummary> = store.values()
                            .filter(|t| {
                                if let Some(since) = req.since {
                                    t.started_at >= since
                                } else {
                                    true
                                }
                            })
                            .map(|t| TraceSummary {
                                correlation_id: t.correlation_id.clone(),
                                started_at: t.started_at,
                                duration_ms: t.duration_ms,
                                span_count: t.span_count,
                                trigger_subject: t.spans.first().map(|s| s.subject.clone()).unwrap_or_default(),
                                last_subject: t.spans.last().map(|s| s.subject.clone()).unwrap_or_default(),
                            })
                            .collect();

                        summaries.sort_by(|a, b| b.started_at.cmp(&a.started_at));
                        summaries.truncate(limit);

                        let response = TraceListResponse { traces: summaries };
                        if let Ok(bytes) = serde_json::to_vec(&response) {
                            let _ = nats.publish_bytes(reply.clone(), bytes).await;
                        }
                    }
                }
            }
        });

        Ok(handle)
    }
}
