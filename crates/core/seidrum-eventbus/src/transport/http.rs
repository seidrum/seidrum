//! HTTP transport server using Axum.
//!
//! Provides HTTP endpoints for publishing events, making requests,
//! managing subscriptions, and health checks.

use crate::bus::{BusMetrics, EventBus, SubscribeOpts};
use crate::delivery::{ChannelConfig, ChannelRegistry, DeliveryChannel, WebhookChannel};
use crate::dispatch::SubscriptionMode;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tower::ServiceBuilder;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tracing::{debug, info, warn};

/// Maximum request body size (2 MiB). Requests exceeding this are rejected
/// with 413 Payload Too Large before the handler is invoked.
const MAX_REQUEST_BODY_SIZE: usize = 2_097_152;

/// Maximum number of webhook subscriptions managed by the HTTP server.
const MAX_HTTP_SUBSCRIPTIONS: usize = 1024;

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

/// Authentication trait for HTTP requests.
///
/// Implement this to add authentication to the HTTP transport server.
/// The authenticator is called once per request before the handler runs.
#[async_trait::async_trait]
pub trait HttpAuthenticator: Send + Sync + 'static {
    /// Authenticate a request from its headers.
    /// Returns `Ok(())` if allowed, or `Err(reason)` if rejected.
    async fn authenticate(&self, headers: &HeaderMap) -> Result<(), String>;
}

/// No-op authenticator that accepts all requests (development only).
pub struct NoHttpAuth;

#[async_trait::async_trait]
impl HttpAuthenticator for NoHttpAuth {
    async fn authenticate(&self, _headers: &HeaderMap) -> Result<(), String> {
        Ok(())
    }
}

/// Application state shared across handlers.
#[derive(Clone)]
pub struct AppState {
    pub bus: Arc<dyn EventBus>,
    pub registry: Arc<ChannelRegistry>,
    pub webhook_channel: Arc<WebhookChannel>,
    pub authenticator: Arc<dyn HttpAuthenticator>,
    pub subscription_count: Arc<AtomicUsize>,
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
    super::DEFAULT_TIMEOUT_MS
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
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub priority: u32,
}

/// Response to subscribe.
#[derive(Debug, Serialize)]
pub struct SubscribeResponse {
    pub id: String,
}

/// Validate and decode a base64 payload, mapping errors to HttpError.
fn validate_and_decode_payload(payload: &str) -> Result<Vec<u8>, HttpError> {
    super::validate_and_decode_payload(payload).map_err(|msg| {
        if msg.contains("exceeds limit") {
            http_error_with_code(StatusCode::PAYLOAD_TOO_LARGE, msg, "PAYLOAD_TOO_LARGE")
        } else {
            http_error(StatusCode::BAD_REQUEST, msg)
        }
    })
}

/// Axum middleware for authentication.
async fn auth_middleware(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, HttpError> {
    state
        .authenticator
        .authenticate(&headers)
        .await
        .map_err(|reason| http_error_with_code(StatusCode::UNAUTHORIZED, reason, "UNAUTHORIZED"))?;
    Ok(next.run(request).await)
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
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
        .layer(
            ServiceBuilder::new()
                .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BODY_SIZE))
                .layer(CorsLayer::permissive()),
        )
}

/// HTTP server for the event bus.
pub struct HttpServer {
    bus: Arc<dyn EventBus>,
    registry: Arc<ChannelRegistry>,
    authenticator: Arc<dyn HttpAuthenticator>,
    shutdown_rx: watch::Receiver<bool>,
}

impl HttpServer {
    /// Create a new HTTP server with no authentication (development only).
    pub fn new(
        bus: Arc<dyn EventBus>,
        registry: Arc<ChannelRegistry>,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            bus,
            registry,
            authenticator: Arc::new(NoHttpAuth),
            shutdown_rx,
        }
    }

    /// Create a new HTTP server with a custom authenticator.
    pub fn with_auth(
        bus: Arc<dyn EventBus>,
        registry: Arc<ChannelRegistry>,
        authenticator: Arc<dyn HttpAuthenticator>,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            bus,
            registry,
            authenticator,
            shutdown_rx,
        }
    }

    /// Start the HTTP server on the given address.
    ///
    /// Runs until the shutdown signal is received or an error occurs.
    pub async fn start(&self, addr: SocketAddr) -> crate::Result<()> {
        let state = AppState {
            bus: Arc::clone(&self.bus),
            registry: Arc::clone(&self.registry),
            webhook_channel: WebhookChannel::new(),
            authenticator: Arc::clone(&self.authenticator),
            subscription_count: Arc::new(AtomicUsize::new(0)),
        };

        let app = create_router(state);

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| crate::EventBusError::Internal(format!("Failed to bind: {}", e)))?;

        info!("HTTP server listening on {}", addr);

        let mut shutdown_rx = self.shutdown_rx.clone();
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.wait_for(|&v| v).await;
            })
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

    let payload_b64 = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(&reply)
    };

    debug!("Request/reply completed for {}", req.subject);

    Ok(Json(RequestResponse {
        payload: payload_b64,
    }))
}

/// Create a webhook subscription.
///
/// Spawns a background delivery task that reads events from the bus subscription
/// and delivers them via HTTP POST to the webhook URL.
async fn create_subscription(
    State(state): State<AppState>,
    Json(req): Json<SubscribeRequest>,
) -> Result<Json<SubscribeResponse>, HttpError> {
    // Atomically reserve a slot. If we exceed the limit, roll back.
    let count = Arc::clone(&state.subscription_count);
    let prev = count.fetch_add(1, Ordering::Relaxed);
    if prev >= MAX_HTTP_SUBSCRIPTIONS {
        count.fetch_sub(1, Ordering::Relaxed);
        return Err(http_error_with_code(
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "Subscription limit reached ({} max)",
                MAX_HTTP_SUBSCRIPTIONS
            ),
            "SUBSCRIPTION_LIMIT",
        ));
    }

    let channel = ChannelConfig::Webhook {
        url: req.url.clone(),
        headers: req.headers.clone(),
    };

    let opts = SubscribeOpts {
        priority: req.priority,
        mode: SubscriptionMode::Async,
        channel: channel.clone(),
        timeout: Duration::from_secs(5),
        filter: None,
    };

    let sub = match state.bus.subscribe(&req.pattern, opts).await {
        Ok(sub) => sub,
        Err(e) => {
            count.fetch_sub(1, Ordering::Relaxed);
            warn!(
                "Subscription creation failed for pattern {}: {}",
                req.pattern, e
            );
            return Err(http_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("{}", e),
            ));
        }
    };

    let sub_id = sub.id.clone();

    // Spawn a delivery task that reads from the subscription receiver
    // and delivers via the webhook channel.
    let webhook = Arc::clone(&state.webhook_channel);
    let delivery_config = channel;
    let task_count = Arc::clone(&count);

    let mut rx = sub.rx;
    let sub_id_for_task = sub_id.clone();
    tokio::spawn(async move {
        debug!(
            "Webhook delivery task started for subscription {}",
            sub_id_for_task
        );
        while let Some(event) = rx.recv().await {
            if let Err(e) = webhook
                .deliver(&event.payload, &event.subject, &delivery_config)
                .await
            {
                warn!(
                    subscription = sub_id_for_task,
                    subject = event.subject,
                    error = %e,
                    "Webhook delivery failed"
                );
            }
        }
        // Decrement when the task exits. This happens when bus.unsubscribe()
        // drops the sender (tx), causing rx.recv() to return None — so
        // DELETE /subscribe/:id triggers cleanup via this path.
        task_count.fetch_sub(1, Ordering::Relaxed);
        debug!(
            "Webhook delivery task ended for subscription {}",
            sub_id_for_task
        );
    });

    debug!(
        "Created subscription {} for pattern {}",
        sub_id, req.pattern
    );

    Ok(Json(SubscribeResponse { id: sub_id }))
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
        assert_eq!(req.timeout_ms, super::super::DEFAULT_TIMEOUT_MS);
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
        let huge = "A".repeat(super::super::MAX_PAYLOAD_SIZE + 1);
        let result = validate_and_decode_payload(&huge);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_payload_invalid_base64() {
        let result = validate_and_decode_payload("not-valid!!!");
        assert!(result.is_err());
    }
}
