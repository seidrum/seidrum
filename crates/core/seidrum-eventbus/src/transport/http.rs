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

    /// Returns `true` if this authenticator does NOT actually verify
    /// requests (e.g. [`NoHttpAuth`]). The HTTP server uses this to
    /// refuse "sensitive" endpoints — `GET /events/:seq` (data
    /// exfiltration vector) — unless [`HttpServer::unsafe_allow_dev_mode`]
    /// has been called explicitly.
    ///
    /// Default `false`. Real authenticator implementations should leave
    /// this default. Only `NoHttpAuth` overrides it to `true`.
    fn is_open(&self) -> bool {
        false
    }
}

/// No-op authenticator that accepts all requests (development only).
///
/// Returns `is_open() == true`, so by default a server using `NoHttpAuth`
/// will refuse `GET /events/:seq` with 401. Tests and dev environments
/// can opt back in via `HttpServer::unsafe_allow_dev_mode()`.
pub struct NoHttpAuth;

#[async_trait::async_trait]
impl HttpAuthenticator for NoHttpAuth {
    async fn authenticate(&self, _headers: &HeaderMap) -> Result<(), String> {
        Ok(())
    }
    fn is_open(&self) -> bool {
        true
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
    /// SSRF validation policy applied to webhook URLs in `/subscribe`
    /// and during persisted-subscription recreation on startup.
    pub webhook_url_policy: crate::delivery::WebhookUrlPolicy,
    /// Mirrors `HttpServer::dev_mode`. When `true`, sensitive endpoints
    /// remain accessible even with an open authenticator.
    pub dev_mode: bool,
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

/// Request to register a webhook-backed sync interceptor (C3).
///
/// The bus will POST events matching `pattern` to `url`. The expected
/// response body is a JSON document of the form:
/// `{"action": "pass" | "drop" | "modify", "payload": "<b64>?"}`.
/// `action == "modify"` requires `payload` to be set; the bus replaces
/// the in-flight event payload with the decoded bytes before continuing.
#[derive(Debug, Deserialize)]
pub struct RegisterInterceptorRequest {
    pub pattern: String,
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Lower priorities run first. Defaults to 100, the same floor that
    /// applies to remote WS interceptors (C1 hardening).
    #[serde(default = "default_interceptor_priority")]
    pub priority: u32,
    /// Per-call timeout in milliseconds. Default 5000ms.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

fn default_interceptor_priority() -> u32 {
    100
}

/// Response to register an interceptor.
#[derive(Debug, Serialize)]
pub struct RegisterInterceptorResponse {
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
        .route("/interceptors", post(create_interceptor))
        .route("/interceptors/{id}", delete(remove_interceptor))
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
    /// SSRF policy applied to webhook URLs at subscription time.
    /// Defaults to `Strict` (https-only, no private hosts).
    webhook_url_policy: crate::delivery::WebhookUrlPolicy,
    /// When `true`, "sensitive" endpoints (currently `GET /events/:seq`)
    /// are reachable even when the configured authenticator is open
    /// (e.g. `NoHttpAuth`). Default `false`. Tests opt in via
    /// `unsafe_allow_dev_mode`. Production deployments leave this off.
    dev_mode: bool,
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
            webhook_url_policy: crate::delivery::WebhookUrlPolicy::Strict,
            dev_mode: false,
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
            webhook_url_policy: crate::delivery::WebhookUrlPolicy::Strict,
            dev_mode: false,
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
            webhook_url_policy: crate::delivery::WebhookUrlPolicy::Strict,
            dev_mode: false,
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

    /// Override the SSRF validation policy for webhook URLs. Default
    /// `Strict` (https-only, no private/loopback). Use `Permissive` only
    /// for tests or trusted networks.
    pub fn with_webhook_url_policy(
        mut self,
        policy: crate::delivery::WebhookUrlPolicy,
    ) -> Self {
        self.webhook_url_policy = policy;
        self
    }

    /// **Dangerous.** Allow access to sensitive endpoints (currently
    /// `GET /events/:seq`) even when the configured authenticator is
    /// open (i.e. [`NoHttpAuth`]). Without this opt-in, an open
    /// authenticator causes those endpoints to return 401.
    ///
    /// Use only in tests or trusted local development. Production
    /// deployments should provide a real [`HttpAuthenticator`] instead.
    pub fn unsafe_allow_dev_mode(mut self) -> Self {
        self.dev_mode = true;
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
                let outcome = recreate_persisted_subscription(
                    &self.bus,
                    &self.webhook_channel,
                    &subscription_count,
                    &bus_to_persisted,
                    &entry,
                    self.webhook_url_policy,
                )
                .await;
                match outcome {
                    RecreateOutcome::Recreated => {}
                    RecreateOutcome::PermanentFailure(reason) => {
                        warn!(
                            persisted_id = entry.persisted_id,
                            pattern = entry.pattern,
                            url = entry.url,
                            reason = reason,
                            "deleting persisted subscription that can no longer be recreated"
                        );
                        if let Err(e) = store.delete_subscription(&entry.persisted_id).await {
                            warn!(
                                persisted_id = entry.persisted_id,
                                error = %e,
                                "failed to garbage-collect permanently-failed persisted subscription"
                            );
                        }
                    }
                    RecreateOutcome::TransientFailure(reason) => {
                        warn!(
                            persisted_id = entry.persisted_id,
                            pattern = entry.pattern,
                            url = entry.url,
                            reason = reason,
                            "transient failure recreating persisted subscription; will retry next restart"
                        );
                    }
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
            webhook_url_policy: self.webhook_url_policy,
            dev_mode: self.dev_mode,
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

/// Outcome of attempting to recreate a persisted entry on startup.
enum RecreateOutcome {
    /// Successfully recreated; the bus subscription id is recorded in
    /// `bus_to_persisted`.
    Recreated,
    /// Permanent failure (URL no longer passes validation, pattern is
    /// no longer accepted, etc). The persisted entry should be deleted
    /// from the store so it doesn't keep retrying on every restart.
    PermanentFailure(String),
    /// Transient failure (e.g. subscription cap reached). The persisted
    /// entry is preserved and will be retried on the next restart.
    TransientFailure(String),
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
///
/// **Permanent vs transient failure handling (H2):** if the persisted URL
/// no longer passes validation (operator tightened SSRF rules across
/// releases) or the pattern is rejected by the current parser, the entry
/// is classified as a permanent failure and `start()` will delete it from
/// the store. Subscription-cap exhaustion is transient and preserves the
/// entry for the next restart.
async fn recreate_persisted_subscription(
    bus: &Arc<dyn EventBus>,
    webhook_channel: &Arc<WebhookChannel>,
    subscription_count: &Arc<AtomicUsize>,
    bus_to_persisted: &Arc<tokio::sync::RwLock<HashMap<String, String>>>,
    entry: &crate::storage::PersistedSubscription,
    policy: crate::delivery::WebhookUrlPolicy,
) -> RecreateOutcome {
    // Re-validate the URL — operator may have tightened the SSRF policy
    // since the entry was persisted.
    if let Err(e) = crate::delivery::validate_webhook_url_with_policy(&entry.url, policy) {
        return RecreateOutcome::PermanentFailure(format!("URL no longer valid: {}", e));
    }

    // Branch on the entry kind: AsyncWebhook subscriptions go through
    // bus.subscribe + a forwarding task; SyncInterceptor entries go
    // through bus.intercept with a WebhookInterceptor proxy.
    match entry.kind {
        crate::storage::PersistedSubscriptionKind::AsyncWebhook => {
            recreate_persisted_async_webhook(
                bus,
                webhook_channel,
                subscription_count,
                bus_to_persisted,
                entry,
            )
            .await
        }
        crate::storage::PersistedSubscriptionKind::SyncInterceptor => {
            recreate_persisted_sync_interceptor(bus, bus_to_persisted, entry).await
        }
    }
}

async fn recreate_persisted_async_webhook(
    bus: &Arc<dyn EventBus>,
    webhook_channel: &Arc<WebhookChannel>,
    subscription_count: &Arc<AtomicUsize>,
    bus_to_persisted: &Arc<tokio::sync::RwLock<HashMap<String, String>>>,
    entry: &crate::storage::PersistedSubscription,
) -> RecreateOutcome {
    // Reserve a slot up front. If we exceed the limit, roll back and bail.
    let prev = subscription_count.fetch_add(1, Ordering::Relaxed);
    if prev >= MAX_HTTP_SUBSCRIPTIONS {
        subscription_count.fetch_sub(1, Ordering::Relaxed);
        return RecreateOutcome::TransientFailure(format!(
            "subscription limit reached ({} max)",
            MAX_HTTP_SUBSCRIPTIONS
        ));
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
        Err(crate::EventBusError::InvalidSubject(msg)) => {
            subscription_count.fetch_sub(1, Ordering::Relaxed);
            return RecreateOutcome::PermanentFailure(format!("invalid pattern: {}", msg));
        }
        Err(e) => {
            subscription_count.fetch_sub(1, Ordering::Relaxed);
            return RecreateOutcome::TransientFailure(format!("{}", e));
        }
    };
    let bus_id = sub.id.clone();

    bus_to_persisted
        .write()
        .await
        .insert(bus_id.clone(), entry.persisted_id.clone());

    spawn_webhook_delivery_task(
        Arc::clone(webhook_channel),
        channel,
        sub.rx,
        bus_id,
        Arc::clone(subscription_count),
    );

    RecreateOutcome::Recreated
}

async fn recreate_persisted_sync_interceptor(
    bus: &Arc<dyn EventBus>,
    bus_to_persisted: &Arc<tokio::sync::RwLock<HashMap<String, String>>>,
    entry: &crate::storage::PersistedSubscription,
) -> RecreateOutcome {
    let interceptor: Arc<dyn crate::dispatch::Interceptor> =
        Arc::new(crate::delivery::WebhookInterceptor::new(
            entry.pattern.clone(),
            entry.url.clone(),
            entry.headers.clone(),
        ));

    let bus_id = match bus
        .intercept(&entry.pattern, entry.priority, interceptor, None)
        .await
    {
        Ok(id) => id,
        Err(crate::EventBusError::InvalidSubject(msg)) => {
            return RecreateOutcome::PermanentFailure(format!("invalid pattern: {}", msg));
        }
        Err(e) => {
            return RecreateOutcome::TransientFailure(format!("{}", e));
        }
    };

    bus_to_persisted
        .write()
        .await
        .insert(bus_id, entry.persisted_id.clone());

    RecreateOutcome::Recreated
}

/// Create a webhook subscription.
///
/// Spawns a background delivery task that reads events from the bus subscription
/// and delivers them via HTTP POST to the webhook URL. If a store is
/// configured, the subscription is also persisted so it survives restart.
///
/// **Validation:** the URL must pass [`crate::delivery::validate_webhook_url`]
/// (https-only scheme, non-private host) before any state is mutated.
async fn create_subscription(
    State(state): State<AppState>,
    Json(req): Json<SubscribeRequest>,
) -> Result<Json<SubscribeResponse>, HttpError> {
    // Validate the webhook URL up front so a failed validation doesn't
    // touch the bus or counter at all. SSRF mitigation (C2).
    if let Err(e) =
        crate::delivery::validate_webhook_url_with_policy(&req.url, state.webhook_url_policy)
    {
        warn!("rejected webhook subscribe with unsafe URL {}: {}", req.url, e);
        return Err(http_error_with_code(
            StatusCode::BAD_REQUEST,
            format!("invalid webhook URL: {}", e),
            "INVALID_WEBHOOK_URL",
        ));
    }

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
            kind: crate::storage::PersistedSubscriptionKind::AsyncWebhook,
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

/// Reject patterns that an HTTP-API caller must not be allowed to
/// intercept. Mirrors the WS server's `validate_remote_interceptor_pattern`.
fn validate_http_interceptor_pattern(pattern: &str) -> Result<(), String> {
    if pattern == ">" {
        return Err("HTTP interceptors cannot register on the catch-all '>' pattern".to_string());
    }
    if pattern.starts_with("_reply.") || pattern == "_reply" {
        return Err(
            "HTTP interceptors cannot register on '_reply.*' (reserved for internal request/reply)"
                .to_string(),
        );
    }
    Ok(())
}

/// Register a webhook-backed sync interceptor (C3 — Phase 4 D6 webhook half).
///
/// Validates the URL via the configured SSRF policy, builds a
/// [`crate::delivery::WebhookInterceptor`], registers it on the bus
/// interceptor chain, and persists the entry so it survives restart.
async fn create_interceptor(
    State(state): State<AppState>,
    Json(req): Json<RegisterInterceptorRequest>,
) -> Result<Json<RegisterInterceptorResponse>, HttpError> {
    // Pattern allowlist (mirrors C1 for the WS path).
    if let Err(reason) = validate_http_interceptor_pattern(&req.pattern) {
        return Err(http_error_with_code(
            StatusCode::BAD_REQUEST,
            reason,
            "INVALID_INTERCEPTOR_PATTERN",
        ));
    }

    // SSRF validation against the configured policy.
    if let Err(e) =
        crate::delivery::validate_webhook_url_with_policy(&req.url, state.webhook_url_policy)
    {
        warn!(
            "rejected interceptor register with unsafe URL {}: {}",
            req.url, e
        );
        return Err(http_error_with_code(
            StatusCode::BAD_REQUEST,
            format!("invalid webhook URL: {}", e),
            "INVALID_WEBHOOK_URL",
        ));
    }

    let timeout = req.timeout_ms.map(Duration::from_millis);
    let mut interceptor = crate::delivery::WebhookInterceptor::new(
        req.pattern.clone(),
        req.url.clone(),
        req.headers.clone(),
    );
    if let Some(t) = timeout {
        interceptor = interceptor.with_timeout(t);
    }
    let interceptor: Arc<dyn crate::dispatch::Interceptor> = Arc::new(interceptor);

    let bus_id = match state
        .bus
        .intercept(&req.pattern, req.priority, interceptor, timeout)
        .await
    {
        Ok(id) => id,
        Err(e) => {
            warn!(
                "Interceptor registration failed for pattern {}: {}",
                req.pattern, e
            );
            return Err(http_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("{}", e),
            ));
        }
    };

    // Persist the interceptor as a SyncInterceptor entry. Failure rolls
    // back the bus registration so the caller sees the error and the
    // bus state stays clean.
    if let Some(store) = &state.store {
        let persisted_id = format!("hi-{}", ulid::Ulid::new());
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
            kind: crate::storage::PersistedSubscriptionKind::SyncInterceptor,
        };
        if let Err(e) = store.save_subscription(&entry).await {
            // Roll back the bus registration before failing the request.
            let _ = state.bus.unsubscribe(&bus_id).await;
            warn!(
                "Failed to persist interceptor for pattern {}: {}",
                req.pattern, e
            );
            return Err(http_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to persist interceptor: {}", e),
            ));
        }
        state
            .bus_to_persisted
            .write()
            .await
            .insert(bus_id.clone(), persisted_id);
    }

    debug!(
        "Registered webhook interceptor {} for pattern {}",
        bus_id, req.pattern
    );

    Ok(Json(RegisterInterceptorResponse { id: bus_id }))
}

/// Remove a webhook-backed sync interceptor previously registered via
/// `POST /interceptors`. Idempotent — returns 204 even if the id is
/// unknown to the bus.
async fn remove_interceptor(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, HttpError> {
    state.bus.unsubscribe(&id).await.map_err(|e| {
        warn!("Interceptor unsubscribe failed for id {}: {}", id, e);
        http_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e))
    })?;

    if let Some(store) = &state.store {
        let persisted_id = state.bus_to_persisted.write().await.remove(&id);
        if let Some(pid) = persisted_id {
            if let Err(e) = store.delete_subscription(&pid).await {
                warn!(
                    bus_id = id,
                    persisted_id = pid,
                    error = %e,
                    "failed to delete persisted interceptor"
                );
            }
        }
    }

    debug!("Removed interceptor {}", id);
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
    // M1: refuse data exfiltration when no real authenticator is wired.
    // Sequences are monotonic from 1, so an open server would let any
    // caller `for seq in 1..N: GET /events/$seq` and dump the entire bus.
    if state.authenticator.is_open() && !state.dev_mode {
        return Err(http_error_with_code(
            StatusCode::UNAUTHORIZED,
            "GET /events/:seq requires a real authenticator (or call HttpServer::unsafe_allow_dev_mode() for tests)",
            "AUTH_REQUIRED",
        ));
    }

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
