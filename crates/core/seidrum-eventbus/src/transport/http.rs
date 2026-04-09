//! HTTP transport server using Axum.
//!
//! Provides HTTP endpoints for publishing events, making requests,
//! managing subscriptions, and health checks.

use crate::bus::{BusMetrics, EventBus, SubscribeOpts};
use crate::delivery::ChannelRegistry;
use crate::dispatch::SubscriptionMode;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, post},
    Json, Router,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceBuilder;
use tower_http::cors::CorsLayer;
use tracing::{debug, info};

/// Application state shared across handlers.
#[derive(Clone)]
pub struct AppState {
    pub bus: Arc<dyn EventBus>,
    pub registry: Arc<ChannelRegistry>,
}

/// Request to publish an event.
#[derive(Debug, Deserialize)]
pub struct PublishRequest {
    pub subject: String,
    pub payload: String, // base64
}

/// Response to publish.
#[derive(Debug, Serialize)]
pub struct PublishResponse {
    pub seq: u64,
}

/// Request to make a request/reply.
#[derive(Debug, Deserialize)]
pub struct RequestRequest {
    pub subject: String,
    pub payload: String, // base64
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    5000
}

/// Response to request.
#[derive(Debug, Serialize)]
pub struct RequestResponse {
    pub payload: String, // base64
}

/// Request to create a webhook subscription.
#[derive(Debug, Deserialize)]
pub struct SubscribeRequest {
    pub pattern: String,
    pub url: String,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub priority: u32,
}

/// Response to subscribe.
#[derive(Debug, Serialize)]
pub struct SubscribeResponse {
    pub id: String,
}

/// Query parameters for event retrieval.
#[derive(Debug, Deserialize)]
pub struct QueryParams {
    pub since: Option<u64>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    100
}

/// Create the HTTP router.
pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/metrics", get(get_metrics))
        .route("/publish", post(publish_event))
        .route("/request", post(make_request))
        .route("/subscribe", post(create_subscription))
        .route("/subscribe/:id", delete(remove_subscription))
        .route("/events/:seq", get(get_event))
        .with_state(state)
        .layer(ServiceBuilder::new().layer(CorsLayer::permissive()))
}

/// HTTP server for the event bus.
pub struct HttpServer {
    bus: Arc<dyn EventBus>,
    registry: Arc<ChannelRegistry>,
}

impl HttpServer {
    /// Create a new HTTP server.
    pub fn new(bus: Arc<dyn EventBus>, registry: Arc<ChannelRegistry>) -> Self {
        Self { bus, registry }
    }

    /// Start the HTTP server on the given address.
    pub async fn start(&self, addr: SocketAddr) -> crate::Result<()> {
        let state = AppState {
            bus: Arc::clone(&self.bus),
            registry: Arc::clone(&self.registry),
        };

        let app = create_router(state);

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| crate::EventBusError::Internal(format!("Failed to bind: {}", e)))?;

        info!("HTTP server listening on {}", addr);

        axum::serve(listener, app)
            .await
            .map_err(|e| crate::EventBusError::Internal(format!("Server error: {}", e)))?;

        Ok(())
    }
}

/// Health check endpoint.
async fn health_check() -> Json<Value> {
    Json(json!({
        "status": "ok"
    }))
}

/// Get metrics endpoint.
async fn get_metrics(State(state): State<AppState>) -> Json<BusMetrics> {
    match state.bus.metrics().await {
        Ok(metrics) => Json(metrics),
        Err(_) => Json(BusMetrics {
            events_published: 0,
            events_delivered: 0,
            events_pending_retry: 0,
            subscription_count: 0,
        }),
    }
}

/// Publish an event.
async fn publish_event(
    State(state): State<AppState>,
    Json(req): Json<PublishRequest>,
) -> Result<Json<PublishResponse>, (StatusCode, String)> {
    let payload = base64::engine::general_purpose::STANDARD
        .decode(&req.payload)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("Base64 decode error: {}", e),
            )
        })?;

    let seq = state
        .bus
        .publish(&req.subject, &payload)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;

    debug!("Published event to {}: seq={}", req.subject, seq);

    Ok(Json(PublishResponse { seq }))
}

/// Make a request and wait for reply.
async fn make_request(
    State(state): State<AppState>,
    Json(req): Json<RequestRequest>,
) -> Result<Json<RequestResponse>, (StatusCode, String)> {
    let payload = base64::engine::general_purpose::STANDARD
        .decode(&req.payload)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("Base64 decode error: {}", e),
            )
        })?;

    let timeout = Duration::from_millis(req.timeout_ms);
    let reply = state
        .bus
        .request(&req.subject, &payload, timeout)
        .await
        .map_err(|e| match e {
            crate::EventBusError::RequestTimeout => {
                (StatusCode::GATEWAY_TIMEOUT, "Request timeout".to_string())
            }
            _ => (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)),
        })?;

    let payload_b64 = base64::engine::general_purpose::STANDARD.encode(&reply);

    debug!("Request/reply completed for {}", req.subject);

    Ok(Json(RequestResponse {
        payload: payload_b64,
    }))
}

/// Create a webhook subscription.
async fn create_subscription(
    State(state): State<AppState>,
    Json(req): Json<SubscribeRequest>,
) -> Result<Json<SubscribeResponse>, (StatusCode, String)> {
    use crate::delivery::ChannelConfig;

    let channel = ChannelConfig::Webhook {
        url: req.url.clone(),
        headers: req.headers.clone(),
    };

    let opts = SubscribeOpts {
        priority: req.priority,
        mode: SubscriptionMode::Async,
        channel,
        timeout: Duration::from_secs(5),
        filter: None,
    };

    let sub = state
        .bus
        .subscribe(&req.pattern, opts)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;

    debug!(
        "Created subscription {} for pattern {}",
        sub.id, req.pattern
    );

    Ok(Json(SubscribeResponse { id: sub.id }))
}

/// Remove a subscription.
async fn remove_subscription(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    state
        .bus
        .unsubscribe(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    debug!("Removed subscription {}", id);
    Ok(StatusCode::NO_CONTENT)
}

/// Get an event by sequence number (stub).
async fn get_event(
    Path(_seq): Path<u64>,
    Query(_params): Query<QueryParams>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // In a real implementation, query the store for the event.
    // For now, return a stub response.
    Err((
        StatusCode::NOT_IMPLEMENTED,
        "Event retrieval not yet implemented".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_publish_request_json() {
        let json = r#"{"subject":"test","payload":"aGVsbG8="}"#;
        let req: PublishRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.subject, "test");
        assert_eq!(req.payload, "aGVsbG8=");
    }

    #[test]
    fn test_subscribe_request_json() {
        let json = r#"{"pattern":"test.*","url":"https://example.com","priority":5}"#;
        let req: SubscribeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.pattern, "test.*");
        assert_eq!(req.url, "https://example.com");
        assert_eq!(req.priority, 5);
    }

    #[test]
    fn test_request_request_default_timeout() {
        let json = r#"{"subject":"test","payload":"aGVsbG8="}"#;
        let req: RequestRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.timeout_ms, 5000);
    }
}
