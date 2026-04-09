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
use tower_http::limit::RequestBodyLimitLayer;
use tracing::{debug, info, warn};

/// Maximum request body size (2 MiB). Requests exceeding this are rejected
/// with 413 Payload Too Large before the handler is invoked.
const MAX_REQUEST_BODY_SIZE: usize = 2_097_152;

/// Maximum base64-encoded payload size within a JSON body (1 MiB).
const MAX_PAYLOAD_SIZE: usize = 1_048_576;

/// Unified error response returned by all HTTP endpoints.
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl ErrorResponse {
    fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            code: None,
        }
    }

    fn with_code(error: impl Into<String>, code: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            code: Some(code.into()),
        }
    }
}

type HttpError = (StatusCode, Json<ErrorResponse>);

fn http_error(status: StatusCode, message: impl Into<String>) -> HttpError {
    (status, Json(ErrorResponse::new(message)))
}

fn http_error_with_code(
    status: StatusCode,
    message: impl Into<String>,
    code: impl Into<String>,
) -> HttpError {
    (status, Json(ErrorResponse::with_code(message, code)))
}

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
    /// Base64-encoded payload.
    pub payload: String,
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
    /// Base64-encoded payload.
    pub payload: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    5000
}

/// Response to request.
#[derive(Debug, Serialize)]
pub struct RequestResponse {
    /// Base64-encoded payload.
    pub payload: String,
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

/// Create the HTTP router with all transport endpoints.
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
        .layer(
            ServiceBuilder::new()
                // Reject request bodies larger than MAX_REQUEST_BODY_SIZE (High #6).
                .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BODY_SIZE))
                // CORS: permissive is suitable for development. For production,
                // configure explicit allowed origins, methods, and headers.
                // TODO(production): Replace CorsLayer::permissive() with restricted policy.
                .layer(CorsLayer::permissive()),
        )
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
    ///
    /// Runs indefinitely until the process is shut down or an error occurs.
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

/// Validate and decode a base64 payload, enforcing size limits.
fn validate_and_decode_payload(payload: &str) -> Result<Vec<u8>, HttpError> {
    if payload.len() > MAX_PAYLOAD_SIZE {
        return Err(http_error_with_code(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "Encoded payload {} bytes exceeds limit of {} bytes",
                payload.len(),
                MAX_PAYLOAD_SIZE
            ),
            "PAYLOAD_TOO_LARGE",
        ));
    }
    base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|e| {
            http_error(
                StatusCode::BAD_REQUEST,
                format!("Base64 decode error: {}", e),
            )
        })
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
) -> Result<Json<PublishResponse>, HttpError> {
    let payload = validate_and_decode_payload(&req.payload)?;

    let seq = state
        .bus
        .publish(&req.subject, &payload)
        .await
        .map_err(|e| {
            warn!("Publish failed for subject {}: {}", req.subject, e);
            http_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e))
        })?;

    debug!("Published event to {}: seq={}", req.subject, seq);

    Ok(Json(PublishResponse { seq }))
}

/// Make a request and wait for reply.
async fn make_request(
    State(state): State<AppState>,
    Json(req): Json<RequestRequest>,
) -> Result<Json<RequestResponse>, HttpError> {
    let payload = validate_and_decode_payload(&req.payload)?;

    let timeout = Duration::from_millis(req.timeout_ms);
    let reply = state
        .bus
        .request(&req.subject, &payload, timeout)
        .await
        .map_err(|e| match e {
            crate::EventBusError::RequestTimeout => http_error_with_code(
                StatusCode::GATEWAY_TIMEOUT,
                "Request timeout",
                "REQUEST_TIMEOUT",
            ),
            _ => {
                warn!("Request failed for subject {}: {}", req.subject, e);
                http_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e))
            }
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
) -> Result<Json<SubscribeResponse>, HttpError> {
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

    let sub = state.bus.subscribe(&req.pattern, opts).await.map_err(|e| {
        warn!(
            "Subscription creation failed for pattern {}: {}",
            req.pattern, e
        );
        http_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e))
    })?;

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
) -> Result<StatusCode, HttpError> {
    state.bus.unsubscribe(&id).await.map_err(|e| {
        warn!("Unsubscribe failed for id {}: {}", id, e);
        http_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e))
    })?;

    debug!("Removed subscription {}", id);
    Ok(StatusCode::NO_CONTENT)
}

/// Get an event by sequence number (stub).
async fn get_event(
    Path(_seq): Path<u64>,
    Query(_params): Query<QueryParams>,
) -> Result<Json<Value>, HttpError> {
    // In a real implementation, query the store for the event.
    // For now, return a stub response.
    Err(http_error_with_code(
        StatusCode::NOT_IMPLEMENTED,
        "Event retrieval not yet implemented",
        "NOT_IMPLEMENTED",
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

    #[test]
    fn test_error_response_serialization() {
        let err = ErrorResponse::new("something went wrong");
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"], "something went wrong");
        assert!(json.get("code").is_none());
    }

    #[test]
    fn test_error_response_with_code() {
        let err = ErrorResponse::with_code("timed out", "REQUEST_TIMEOUT");
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"], "timed out");
        assert_eq!(json["code"], "REQUEST_TIMEOUT");
    }

    #[test]
    fn test_validate_payload_ok() {
        let result = validate_and_decode_payload("aGVsbG8=");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), b"hello");
    }

    #[test]
    fn test_validate_payload_too_large() {
        let huge = "A".repeat(MAX_PAYLOAD_SIZE + 1);
        let result = validate_and_decode_payload(&huge);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_payload_invalid_base64() {
        let result = validate_and_decode_payload("not-valid!!!");
        assert!(result.is_err());
    }
}
