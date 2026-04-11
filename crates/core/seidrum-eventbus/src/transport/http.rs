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
    /// Storage backend for persisting webhook subscriptions across
    /// restarts. `None` means subscription persistence is disabled
    /// (current behaviour for `HttpServer::new` / `with_auth`); the
    /// builder always provides a real store via `with_webhook_channel`.
    pub store: Option<Arc<dyn crate::storage::EventStore>>,
    /// Map of bus subscription id → persisted subscription id. Used by
    /// `DELETE /subscribe/:id` to find the persisted entry to delete.
    pub bus_to_persisted: Arc<tokio::sync::RwLock<std::collections::HashMap<String, String>>>,
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
        .route("/subscribe/{id}", delete(remove_subscription))
        .route("/events/{seq}", get(get_event))
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
    webhook_channel: Arc<WebhookChannel>,
    authenticator: Arc<dyn HttpAuthenticator>,
    shutdown_rx: watch::Receiver<bool>,
    /// Optional storage backend for webhook subscription persistence.
    /// When set, `POST /subscribe` saves the config and `start()` will
    /// recreate all persisted subscriptions before serving requests.
    store: Option<Arc<dyn crate::storage::EventStore>>,
}

impl HttpServer {
    /// Create a new HTTP server with no authentication (development only).
    /// Constructs its own `WebhookChannel`.
    ///
    /// **Deprecated:** This constructor produces an HTTP server whose
    /// `WebhookChannel` instance is independent from the dispatch engine's,
    /// so retry-time and live deliveries cannot share state (connection
    /// pools, future stats). Use [`crate::EventBusBuilder::with_http`] to
    /// have the builder wire up a single shared channel, or
    /// [`Self::with_webhook_channel`] for manual wiring.
    #[deprecated(
        since = "0.2.0",
        note = "use EventBusBuilder::with_http or HttpServer::with_webhook_channel \
                so the engine and HTTP transport share a single WebhookChannel"
    )]
    pub fn new(
        bus: Arc<dyn EventBus>,
        registry: Arc<ChannelRegistry>,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            bus,
            registry,
            webhook_channel: WebhookChannel::new(),
            authenticator: Arc::new(NoHttpAuth),
            shutdown_rx,
            store: None,
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
            webhook_channel: WebhookChannel::new(),
            authenticator,
            shutdown_rx,
            store: None,
        }
    }

    /// Create a new HTTP server with an explicit shared `WebhookChannel`.
    /// Used by [`crate::EventBusBuilder`] so the dispatch engine and the
    /// HTTP transport share the same channel instance.
    pub fn with_webhook_channel(
        bus: Arc<dyn EventBus>,
        registry: Arc<ChannelRegistry>,
        webhook_channel: Arc<WebhookChannel>,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            bus,
            registry,
            webhook_channel,
            authenticator: Arc::new(NoHttpAuth),
            shutdown_rx,
            store: None,
        }
    }

    /// Attach a storage backend for webhook subscription persistence.
    /// Builder pattern — chain after `new` / `with_auth` / `with_webhook_channel`.
    /// When set, `POST /subscribe` saves the config to storage and
    /// `start()` recreates persisted subscriptions on startup.
    pub fn with_store(mut self, store: Arc<dyn crate::storage::EventStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Start the HTTP server on the given address.
    ///
    /// Runs until the shutdown signal is received or an error occurs.
    /// If a store was configured via `with_store()`, all persisted webhook
    /// subscriptions are recreated against the bus before serving requests.
    pub async fn start(&self, addr: SocketAddr) -> crate::Result<()> {
        let subscription_count = Arc::new(AtomicUsize::new(0));
        let bus_to_persisted: Arc<
            tokio::sync::RwLock<std::collections::HashMap<String, String>>,
        > = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));

        // Recreate persisted subscriptions from the store. Each persisted
        // entry becomes a fresh bus subscription with a new bus id; the
        // mapping bus_id → persisted_id is recorded so DELETE can clean up
        // both the bus subscription and the persisted entry.
        if let Some(store) = &self.store {
            let persisted = store.list_subscriptions().await.map_err(|e| {
                crate::EventBusError::Internal(format!(
                    "failed to list persisted subscriptions: {}",
                    e
                ))
            })?;
            if !persisted.is_empty() {
                info!(
                    "Recreating {} persisted webhook subscriptions",
                    persisted.len()
                );
            }
            for entry in persisted {
                if let Err(e) = recreate_persisted_subscription(
                    &self.bus,
                    &self.webhook_channel,
                    &subscription_count,
                    &bus_to_persisted,
                    &entry,
                )
                .await
                {
                    warn!(
                        persisted_id = entry.persisted_id,
                        pattern = entry.pattern,
                        url = entry.url,
                        error = %e,
                        "failed to recreate persisted subscription"
                    );
                }
            }
        }

        let state = AppState {
            bus: Arc::clone(&self.bus),
            registry: Arc::clone(&self.registry),
            webhook_channel: Arc::clone(&self.webhook_channel),
            authenticator: Arc::clone(&self.authenticator),
            subscription_count,
            store: self.store.clone(),
            bus_to_persisted,
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

/// Spawn the background delivery task that pulls events from a webhook
/// subscription's receive channel and POSTs them to the configured URL.
///
/// The task exits when `rx.recv()` returns `None` — which happens when the
/// bus subscription is dropped (via `unsubscribe`). On exit it decrements
/// `task_count` so the slot becomes available again.
fn spawn_webhook_delivery_task(
    webhook: Arc<WebhookChannel>,
    delivery_config: ChannelConfig,
    mut rx: tokio::sync::mpsc::Receiver<crate::request_reply::DispatchedEvent>,
    sub_id: String,
    task_count: Arc<AtomicUsize>,
) {
    tokio::spawn(async move {
        debug!("Webhook delivery task started for subscription {}", sub_id);
        while let Some(event) = rx.recv().await {
            if let Err(e) = webhook
                .deliver(&event.payload, &event.subject, &delivery_config)
                .await
            {
                warn!(
                    subscription = sub_id,
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
        debug!("Webhook delivery task ended for subscription {}", sub_id);
    });
}

/// Recreate a persisted webhook subscription against the bus.
///
/// Called once per persisted entry when the HTTP server starts up. The
/// recreated subscription gets a fresh runtime bus id (different from the
/// previous run); the bus id → persisted id mapping is recorded so the
/// DELETE handler can find and remove the persisted entry.
///
/// Counts toward `MAX_HTTP_SUBSCRIPTIONS`, so a corrupted store with more
/// than the limit will simply skip the overflow entries (logged at warn).
async fn recreate_persisted_subscription(
    bus: &Arc<dyn EventBus>,
    webhook_channel: &Arc<WebhookChannel>,
    subscription_count: &Arc<AtomicUsize>,
    bus_to_persisted: &Arc<tokio::sync::RwLock<HashMap<String, String>>>,
    entry: &crate::storage::PersistedSubscription,
) -> crate::Result<String> {
    // Reserve a slot up front. If we exceed the limit, roll back and bail.
    let prev = subscription_count.fetch_add(1, Ordering::Relaxed);
    if prev >= MAX_HTTP_SUBSCRIPTIONS {
        subscription_count.fetch_sub(1, Ordering::Relaxed);
        return Err(crate::EventBusError::Internal(format!(
            "subscription limit reached ({} max)",
            MAX_HTTP_SUBSCRIPTIONS
        )));
    }

    let channel = ChannelConfig::Webhook {
        url: entry.url.clone(),
        headers: entry.headers.clone(),
    };
    let opts = SubscribeOpts {
        priority: entry.priority,
        mode: SubscriptionMode::Async,
        channel: channel.clone(),
        timeout: Duration::from_secs(5),
        filter: None,
    };

    let sub = match bus.subscribe(&entry.pattern, opts).await {
        Ok(sub) => sub,
        Err(e) => {
            subscription_count.fetch_sub(1, Ordering::Relaxed);
            return Err(e);
        }
    };
    let bus_id = sub.id.clone();

    // Record the mapping so DELETE can find the persisted entry by bus id.
    bus_to_persisted
        .write()
        .await
        .insert(bus_id.clone(), entry.persisted_id.clone());

    spawn_webhook_delivery_task(
        Arc::clone(webhook_channel),
        channel,
        sub.rx,
        bus_id.clone(),
        Arc::clone(subscription_count),
    );

    Ok(bus_id)
}

/// Create a webhook subscription.
///
/// Spawns a background delivery task that reads events from the bus subscription
/// and delivers them via HTTP POST to the webhook URL. If a store is
/// configured, the subscription is also persisted so it survives restart.
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

    // Persist the subscription if a store is configured. We persist BEFORE
    // spawning the delivery task so that if persistence fails the caller
    // sees the error and the bus subscription is rolled back.
    if let Some(store) = &state.store {
        let persisted_id = format!("ws-{}", ulid::Ulid::new());
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let entry = crate::storage::PersistedSubscription {
            persisted_id: persisted_id.clone(),
            pattern: req.pattern.clone(),
            url: req.url.clone(),
            headers: req.headers.clone(),
            priority: req.priority,
            created_at: now_ms,
        };
        if let Err(e) = store.save_subscription(&entry).await {
            // Roll back the bus subscription before failing the request.
            let _ = state.bus.unsubscribe(&sub_id).await;
            count.fetch_sub(1, Ordering::Relaxed);
            warn!(
                "Failed to persist subscription for pattern {}: {}",
                req.pattern, e
            );
            return Err(http_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to persist subscription: {}", e),
            ));
        }
        state
            .bus_to_persisted
            .write()
            .await
            .insert(sub_id.clone(), persisted_id);
    }

    spawn_webhook_delivery_task(
        Arc::clone(&state.webhook_channel),
        channel,
        sub.rx,
        sub_id.clone(),
        Arc::clone(&count),
    );

    debug!(
        "Created subscription {} for pattern {}",
        sub_id, req.pattern
    );

    Ok(Json(SubscribeResponse { id: sub_id }))
}

/// Remove a subscription.
///
/// Unsubscribes from the bus and, if a store is configured, also deletes
/// the persisted entry so it does not get recreated on next restart.
async fn remove_subscription(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, HttpError> {
    state.bus.unsubscribe(&id).await.map_err(|e| {
        warn!("Unsubscribe failed for id {}: {}", id, e);
        http_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e))
    })?;

    // If this was a persisted subscription, drop the persisted entry too.
    if let Some(store) = &state.store {
        let persisted_id = state.bus_to_persisted.write().await.remove(&id);
        if let Some(pid) = persisted_id {
            if let Err(e) = store.delete_subscription(&pid).await {
                // Log but do not fail the request — the bus unsubscribe
                // already succeeded; the orphaned persisted entry will be
                // cleaned up on a future restart attempt or manually.
                warn!(
                    bus_id = id,
                    persisted_id = pid,
                    error = %e,
                    "failed to delete persisted subscription"
                );
            }
        }
    }

    debug!("Removed subscription {}", id);
    Ok(StatusCode::NO_CONTENT)
}

/// Get a stored event by its sequence number.
///
/// Returns the full `StoredEvent` (subject, payload, status, deliveries,
/// reply_subject) as JSON. The payload is base64-encoded for safe transport
/// inside JSON. Returns 404 if no event with the given seq exists or has
/// been compacted away.
async fn get_event(
    State(state): State<AppState>,
    Path(seq): Path<u64>,
) -> Result<Json<Value>, HttpError> {
    let event = state.bus.get_event(seq).await.map_err(|e| {
        warn!("get_event failed for seq {}: {}", seq, e);
        http_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e))
    })?;
    let event = match event {
        Some(e) => e,
        None => {
            return Err(http_error_with_code(
                StatusCode::NOT_FOUND,
                format!("event {} not found", seq),
                "EVENT_NOT_FOUND",
            ));
        }
    };

    // Encode the payload as base64 for JSON-safe transport. The rest of
    // StoredEvent serializes via serde.
    use base64::Engine;
    let payload_b64 = base64::engine::general_purpose::STANDARD.encode(&event.payload);
    let body = json!({
        "seq": event.seq,
        "subject": event.subject,
        "payload": payload_b64,
        "stored_at": event.stored_at,
        "status": event.status,
        "deliveries": event.deliveries,
        "reply_subject": event.reply_subject,
    });
    Ok(Json(body))
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
