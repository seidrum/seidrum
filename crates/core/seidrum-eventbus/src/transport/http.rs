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
    /// exfiltration vector) — unless [`HttpServer::unsafe_allow_http_dev_mode`]
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
/// can opt back in via `HttpServer::unsafe_allow_http_dev_mode()`.
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
    /// Counter for HTTP-registered sync interceptors. Capped at
    /// MAX_HTTP_INTERCEPTORS so an attacker can't fill the interceptor
    /// table or unbounded-grow the redb subscriptions table via
    /// `POST /interceptors` (B3).
    pub interceptor_count: Arc<AtomicUsize>,
    /// Set of bus subscription ids that were registered through
    /// `POST /interceptors` (i.e. SyncInterceptor kind). Used by
    /// `DELETE /interceptors/:id` to refuse cross-type deletion of an
    /// async webhook subscription via the wrong endpoint, and to
    /// keep `interceptor_count` consistent. Mirrors a similar set
    /// for subscriptions tracked separately. (F4 fix.)
    pub interceptor_ids: Arc<tokio::sync::RwLock<std::collections::HashSet<String>>>,
    /// Set of bus subscription ids that were registered through
    /// `POST /subscribe` (AsyncWebhook kind). Used by
    /// `DELETE /subscribe/:id` for symmetric cross-type protection.
    pub subscription_ids: Arc<tokio::sync::RwLock<std::collections::HashSet<String>>>,
    /// **F13** Rate limiter for state-changing registration ops.
    /// Tuple is (window_start_unix_secs, request_count_in_window).
    pub registration_rate: Arc<std::sync::Mutex<(u64, u64)>>,
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
    /// `unsafe_allow_http_dev_mode`. Production deployments leave this off.
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
    pub fn with_webhook_url_policy(mut self, policy: crate::delivery::WebhookUrlPolicy) -> Self {
        self.webhook_url_policy = policy;
        self
    }

    /// **Dangerous.** Allow access to sensitive endpoints (currently
    /// `GET /events/:seq`, `POST /subscribe`, `POST /interceptors`)
    /// even when the configured authenticator is open
    /// (i.e. [`NoHttpAuth`]). Without this opt-in, an open
    /// authenticator causes those endpoints to return 401.
    ///
    /// Use only in tests or trusted local development. Production
    /// deployments should provide a real [`HttpAuthenticator`] instead.
    ///
    /// Named to match `EventBusBuilder::unsafe_allow_http_dev_mode` so
    /// the two opt-ins are visually consistent (N11).
    pub fn unsafe_allow_http_dev_mode(mut self) -> Self {
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
        let interceptor_count = Arc::new(AtomicUsize::new(0));
        let bus_to_persisted: Arc<tokio::sync::RwLock<std::collections::HashMap<String, String>>> =
            Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        let interceptor_ids: Arc<tokio::sync::RwLock<std::collections::HashSet<String>>> =
            Arc::new(tokio::sync::RwLock::new(std::collections::HashSet::new()));
        let subscription_ids: Arc<tokio::sync::RwLock<std::collections::HashSet<String>>> =
            Arc::new(tokio::sync::RwLock::new(std::collections::HashSet::new()));

        // F12: warn at startup if dev mode was opted into. Operators
        // who left this on accidentally see it on every boot.
        if self.dev_mode {
            warn!(
                "HTTP server starting with unsafe_allow_http_dev_mode=TRUE — \
                 sensitive endpoints (POST /subscribe, POST /interceptors, \
                 GET /events/:seq) are accessible without a real authenticator. \
                 Disable in production."
            );
        }

        // Recreate persisted subscriptions from the store. Each persisted
        // entry becomes a fresh bus subscription with a new bus id; the
        // mapping bus_id → persisted_id is recorded so DELETE can clean up
        // both the bus subscription and the persisted entry.
        //
        // **N5 fix:** if list_subscriptions returns an error (e.g. the
        // operator wired a custom EventStore that doesn't implement the
        // Phase-4 subscription methods), warn loudly and continue with
        // zero recreated subscriptions. The previous default of
        // `Ok(vec![])` silently masked this state.
        if let Some(store) = &self.store {
            let persisted = match store.list_subscriptions().await {
                Ok(v) => v,
                Err(e) => {
                    warn!(
                        error = %e,
                        "EventStore does not support subscription persistence — \
                         starting with zero recreated webhook subscriptions. \
                         POST /subscribe and POST /interceptors will return 500 \
                         until a store with persistence is wired."
                    );
                    Vec::new()
                }
            };
            if !persisted.is_empty() {
                info!(
                    "Recreating {} persisted webhook subscriptions/interceptors",
                    persisted.len()
                );
            }
            for entry in persisted {
                let outcome = recreate_persisted_subscription(
                    &self.bus,
                    &self.webhook_channel,
                    &subscription_count,
                    &interceptor_count,
                    &bus_to_persisted,
                    &subscription_ids,
                    &interceptor_ids,
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
                            "deleting persisted entry that can no longer be recreated"
                        );
                        if let Err(e) = store.delete_subscription(&entry.persisted_id).await {
                            warn!(
                                persisted_id = entry.persisted_id,
                                error = %e,
                                "failed to garbage-collect permanently-failed persisted entry"
                            );
                        }
                    }
                    RecreateOutcome::TransientFailure(reason) => {
                        warn!(
                            persisted_id = entry.persisted_id,
                            pattern = entry.pattern,
                            url = entry.url,
                            reason = reason,
                            "transient failure recreating persisted entry; will retry next restart"
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
            interceptor_count,
            store: self.store.clone(),
            bus_to_persisted,
            interceptor_ids,
            subscription_ids,
            registration_rate: Arc::new(std::sync::Mutex::new((0, 0))),
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
#[allow(clippy::too_many_arguments)]
async fn recreate_persisted_subscription(
    bus: &Arc<dyn EventBus>,
    webhook_channel: &Arc<WebhookChannel>,
    subscription_count: &Arc<AtomicUsize>,
    interceptor_count: &Arc<AtomicUsize>,
    bus_to_persisted: &Arc<tokio::sync::RwLock<HashMap<String, String>>>,
    subscription_ids: &Arc<tokio::sync::RwLock<std::collections::HashSet<String>>>,
    interceptor_ids: &Arc<tokio::sync::RwLock<std::collections::HashSet<String>>>,
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
                subscription_ids,
                entry,
            )
            .await
        }
        crate::storage::PersistedSubscriptionKind::SyncInterceptor => {
            recreate_persisted_sync_interceptor(
                bus,
                interceptor_count,
                bus_to_persisted,
                interceptor_ids,
                entry,
                policy,
            )
            .await
        }
    }
}

async fn recreate_persisted_async_webhook(
    bus: &Arc<dyn EventBus>,
    webhook_channel: &Arc<WebhookChannel>,
    subscription_count: &Arc<AtomicUsize>,
    bus_to_persisted: &Arc<tokio::sync::RwLock<HashMap<String, String>>>,
    subscription_ids: &Arc<tokio::sync::RwLock<std::collections::HashSet<String>>>,
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
    subscription_ids.write().await.insert(bus_id.clone());

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
    interceptor_count: &Arc<AtomicUsize>,
    bus_to_persisted: &Arc<tokio::sync::RwLock<HashMap<String, String>>>,
    interceptor_ids: &Arc<tokio::sync::RwLock<std::collections::HashSet<String>>>,
    entry: &crate::storage::PersistedSubscription,
    policy: crate::delivery::WebhookUrlPolicy,
) -> RecreateOutcome {
    // === N4 hardening ===
    // Re-run the same validation that POST /interceptors enforces, so a
    // policy that was tightened across releases can't be bypassed by
    // entries persisted under the older policy.

    if let Err(e) = crate::transport::validate_remote_pattern(&entry.pattern) {
        return RecreateOutcome::PermanentFailure(format!("pattern no longer permitted: {}", e));
    }
    let effective_priority = crate::transport::clamp_remote_interceptor_priority(entry.priority);
    let clamped_timeout_ms = crate::transport::clamp_remote_interceptor_timeout(entry.timeout_ms);

    // Reserve a slot up front. If we exceed the cap, classify as
    // transient — the entry stays in storage and will be retried on
    // the next restart (when other entries may have been deleted).
    let prev = interceptor_count.fetch_add(1, Ordering::Relaxed);
    if prev >= MAX_HTTP_INTERCEPTORS {
        interceptor_count.fetch_sub(1, Ordering::Relaxed);
        return RecreateOutcome::TransientFailure(format!(
            "interceptor cap reached ({} max)",
            MAX_HTTP_INTERCEPTORS
        ));
    }

    let mut interceptor = crate::delivery::WebhookInterceptor::with_policy(
        entry.pattern.clone(),
        entry.url.clone(),
        entry.headers.clone(),
        policy,
    );
    if let Some(ms) = clamped_timeout_ms {
        interceptor = interceptor.with_timeout(Duration::from_millis(ms));
    }
    let interceptor: Arc<dyn crate::dispatch::Interceptor> = Arc::new(interceptor);

    let engine_timeout = clamped_timeout_ms.map(Duration::from_millis);
    let bus_id = match bus
        .intercept(
            &entry.pattern,
            effective_priority,
            interceptor,
            engine_timeout,
        )
        .await
    {
        Ok(id) => id,
        Err(crate::EventBusError::InvalidSubject(msg)) => {
            interceptor_count.fetch_sub(1, Ordering::Relaxed);
            return RecreateOutcome::PermanentFailure(format!("invalid pattern: {}", msg));
        }
        Err(e) => {
            interceptor_count.fetch_sub(1, Ordering::Relaxed);
            return RecreateOutcome::TransientFailure(format!("{}", e));
        }
    };

    bus_to_persisted
        .write()
        .await
        .insert(bus_id.clone(), entry.persisted_id.clone());
    interceptor_ids.write().await.insert(bus_id);

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
    // B4: refuse state-changing webhook registration under open auth.
    refuse_if_unauth_state_changing(&state, "POST /subscribe")?;
    // F13: rate limit before any work.
    check_registration_rate_limit(&state)?;

    // F1: token-aware pattern validation. Prior to F1 only the
    // interceptor path was hardened — POST /subscribe accepted
    // `_reply.>` / `*.>` patterns and let an attacker hijack every
    // reply on the bus by registering a webhook subscription on the
    // matching wildcard. The same shared validator now applies here.
    if let Err(e) = crate::transport::validate_remote_pattern(&req.pattern) {
        return Err(http_error_with_code(
            StatusCode::BAD_REQUEST,
            e.to_string(),
            "INVALID_SUBSCRIBE_PATTERN",
        ));
    }

    // Validate the webhook URL up front so a failed validation doesn't
    // touch the bus or counter at all. SSRF mitigation (C2).
    if let Err(e) =
        crate::delivery::validate_webhook_url_with_policy(&req.url, state.webhook_url_policy)
    {
        // N3: log specifics server-side, return generic to caller.
        warn!(
            "rejected webhook subscribe with unsafe URL {}: {}",
            req.url, e
        );
        return Err(http_error_invalid_webhook_url());
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

    // Persist the subscription if a store is configured. **N6 ordering:**
    // we insert into bus_to_persisted BEFORE save_subscription so that a
    // racing DELETE always finds the mapping; if save fails we delete
    // both the mapping and the bus subscription before returning.
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
            timeout_ms: None,
        };

        state
            .bus_to_persisted
            .write()
            .await
            .insert(sub_id.clone(), persisted_id.clone());
        // F4: track this id as an AsyncWebhook subscription so DELETE
        // can refuse cross-type deletion.
        state.subscription_ids.write().await.insert(sub_id.clone());

        if let Err(e) = store.save_subscription(&entry).await {
            // Roll back the mapping, the type set, and the bus subscription.
            state.bus_to_persisted.write().await.remove(&sub_id);
            state.subscription_ids.write().await.remove(&sub_id);
            if let Err(unsub_err) = state.bus.unsubscribe(&sub_id).await {
                warn!(
                    sub_id = %sub_id,
                    error = %unsub_err,
                    "rollback unsubscribe failed after save_subscription error"
                );
            }
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
    } else {
        // No store configured — still track the bus id so DELETE can
        // refuse cross-type deletion even without persistence.
        state.subscription_ids.write().await.insert(sub_id.clone());
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
///
/// **F4 fix:** refuses to delete an id that wasn't registered as an
/// AsyncWebhook subscription via this endpoint. Cross-type deletion
/// (e.g. trying to delete a SyncInterceptor via DELETE /subscribe/:id)
/// returns 404 instead of corrupting counters.
async fn remove_subscription(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, HttpError> {
    // F4: refuse cross-type deletion. The set is checked-and-removed
    // atomically so a concurrent DELETE on the same id only succeeds
    // for one caller.
    let was_present = state.subscription_ids.write().await.remove(&id);
    if !was_present {
        return Err(http_error_with_code(
            StatusCode::NOT_FOUND,
            format!("subscription {} not found", id),
            "SUBSCRIPTION_NOT_FOUND",
        ));
    }

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

/// Maximum number of HTTP-registered sync interceptors a server may hold.
/// Independent of subscription count because interceptors run on every
/// matching publish whereas subscriptions just receive events.
const MAX_HTTP_INTERCEPTORS: usize = 64;

/// **F13 rate limit**: maximum number of state-changing webhook
/// registration requests (`POST /subscribe` + `POST /interceptors` +
/// the matching DELETEs) allowed per
/// `WEBHOOK_REGISTRATION_RATE_WINDOW_SECS` window. Combined with
/// the per-server caps (B3 / MAX_HTTP_SUBSCRIPTIONS) this prevents
/// the persistence layer from being hammered by a POST/DELETE loop.
const WEBHOOK_REGISTRATION_RATE_LIMIT: u64 = 100;
const WEBHOOK_REGISTRATION_RATE_WINDOW_SECS: u64 = 60;

/// Refuse a state-changing webhook-registration request when the
/// configured authenticator is open (e.g. [`NoHttpAuth`]) and dev mode
/// is not explicitly enabled. Used by `POST /subscribe` and
/// `POST /interceptors`.
///
/// **B4 fix:** these endpoints install persistent webhook deliveries
/// that survive restart. Allowing them under `NoHttpAuth` lets an
/// unauthenticated attacker siphon every matching event for the life
/// of the process.
fn refuse_if_unauth_state_changing(state: &AppState, op_name: &str) -> Result<(), HttpError> {
    if state.authenticator.is_open() && !state.dev_mode {
        return Err(http_error_with_code(
            StatusCode::UNAUTHORIZED,
            format!(
                "{} requires a real authenticator (or HttpServer::unsafe_allow_http_dev_mode() for tests)",
                op_name
            ),
            "AUTH_REQUIRED",
        ));
    }
    Ok(())
}

/// Map a [`crate::delivery::WebhookUrlError`] to a generic 400 response
/// that does NOT leak resolved IPs or DNS specifics. Server-side
/// callers log the underlying error themselves before invoking this.
///
/// **N3 fix:** the previous error format included the resolved IP
/// (`URL host internal.example.com resolves to 10.0.4.17`), letting an
/// attacker enumerate internal DNS topology via the response body.
fn http_error_invalid_webhook_url() -> HttpError {
    http_error_with_code(
        StatusCode::BAD_REQUEST,
        "invalid webhook URL",
        "INVALID_WEBHOOK_URL",
    )
}

/// **F13 rate limiter**: enforce a sliding-ish-window cap on
/// state-changing registration ops. The window resets when the
/// current time crosses a `WEBHOOK_REGISTRATION_RATE_WINDOW_SECS`
/// boundary; counts within a window are summed. Crude but effective
/// against the POST/DELETE write-amplification loop.
fn check_registration_rate_limit(state: &AppState) -> Result<(), HttpError> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let window_idx = now_secs / WEBHOOK_REGISTRATION_RATE_WINDOW_SECS;

    let mut guard = state.registration_rate.lock().unwrap();
    let (current_window, count) = *guard;
    if current_window != window_idx {
        // New window — reset.
        *guard = (window_idx, 1);
        return Ok(());
    }
    if count >= WEBHOOK_REGISTRATION_RATE_LIMIT {
        return Err(http_error_with_code(
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "registration rate limit reached ({} req/{}s)",
                WEBHOOK_REGISTRATION_RATE_LIMIT, WEBHOOK_REGISTRATION_RATE_WINDOW_SECS
            ),
            "REGISTRATION_RATE_LIMIT",
        ));
    }
    *guard = (window_idx, count + 1);
    Ok(())
}

/// Register a webhook-backed sync interceptor (C3 — Phase 4 D6 webhook half).
///
/// Hardening (post-re-review):
/// - **B4** auth gate (refuses under open auth without dev mode)
/// - **B1** shared token-aware pattern validator (rejects `*.>`, `*.*`, etc.)
/// - **B2** priority floor at `MIN_REMOTE_INTERCEPTOR_PRIORITY`
/// - **B3** per-server cap at `MAX_HTTP_INTERCEPTORS`
/// - **N2** timeout clamp at `MAX_REMOTE_INTERCEPTOR_TIMEOUT_MS`
/// - **N3** generic SSRF error response (no DNS leak)
/// - **N6** mapping inserted before `save_subscription`, rolled back on failure
async fn create_interceptor(
    State(state): State<AppState>,
    Json(req): Json<RegisterInterceptorRequest>,
) -> Result<Json<RegisterInterceptorResponse>, HttpError> {
    // B4: refuse state-changing webhook registration under open auth.
    refuse_if_unauth_state_changing(&state, "POST /interceptors")?;
    // F13: rate limit before any work.
    check_registration_rate_limit(&state)?;

    // B1: shared token-aware pattern validator.
    if let Err(e) = crate::transport::validate_remote_pattern(&req.pattern) {
        return Err(http_error_with_code(
            StatusCode::BAD_REQUEST,
            e.to_string(),
            "INVALID_INTERCEPTOR_PATTERN",
        ));
    }

    // C2/N3: SSRF validation against the configured policy. Log specifics
    // server-side; return generic to caller.
    if let Err(e) =
        crate::delivery::validate_webhook_url_with_policy(&req.url, state.webhook_url_policy)
    {
        warn!(
            "rejected interceptor register with unsafe URL {}: {}",
            req.url, e
        );
        return Err(http_error_invalid_webhook_url());
    }

    // B2: priority floor.
    let effective_priority = crate::transport::clamp_remote_interceptor_priority(req.priority);
    // N2: timeout clamp.
    let clamped_timeout_ms = crate::transport::clamp_remote_interceptor_timeout(req.timeout_ms);

    // B3: reserve a slot up front. Roll back on any subsequent failure.
    let icount = Arc::clone(&state.interceptor_count);
    let prev = icount.fetch_add(1, Ordering::Relaxed);
    if prev >= MAX_HTTP_INTERCEPTORS {
        icount.fetch_sub(1, Ordering::Relaxed);
        return Err(http_error_with_code(
            StatusCode::TOO_MANY_REQUESTS,
            format!("Interceptor limit reached ({} max)", MAX_HTTP_INTERCEPTORS),
            "INTERCEPTOR_LIMIT",
        ));
    }

    let timeout = clamped_timeout_ms.map(Duration::from_millis);
    let mut interceptor = crate::delivery::WebhookInterceptor::with_policy(
        req.pattern.clone(),
        req.url.clone(),
        req.headers.clone(),
        state.webhook_url_policy,
    );
    if let Some(t) = timeout {
        interceptor = interceptor.with_timeout(t);
    }
    let interceptor: Arc<dyn crate::dispatch::Interceptor> = Arc::new(interceptor);

    let bus_id = match state
        .bus
        .intercept(&req.pattern, effective_priority, interceptor, timeout)
        .await
    {
        Ok(id) => id,
        Err(e) => {
            icount.fetch_sub(1, Ordering::Relaxed);
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

    // Persist the interceptor as a SyncInterceptor entry. **N6 ordering:**
    // insert mapping first, then save; on failure roll back both the
    // mapping and the bus registration AND the cap counter.
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
            priority: effective_priority,
            created_at: now_ms,
            kind: crate::storage::PersistedSubscriptionKind::SyncInterceptor,
            timeout_ms: clamped_timeout_ms,
        };

        state
            .bus_to_persisted
            .write()
            .await
            .insert(bus_id.clone(), persisted_id.clone());
        // F4: track this id as a SyncInterceptor so DELETE can refuse
        // cross-type deletion.
        state.interceptor_ids.write().await.insert(bus_id.clone());

        if let Err(e) = store.save_subscription(&entry).await {
            // Roll back mapping, type set, bus registration, cap counter.
            state.bus_to_persisted.write().await.remove(&bus_id);
            state.interceptor_ids.write().await.remove(&bus_id);
            if let Err(unsub_err) = state.bus.unsubscribe(&bus_id).await {
                warn!(
                    bus_id = %bus_id,
                    error = %unsub_err,
                    "rollback unsubscribe failed after interceptor save error"
                );
            }
            icount.fetch_sub(1, Ordering::Relaxed);
            warn!(
                "Failed to persist interceptor for pattern {}: {}",
                req.pattern, e
            );
            return Err(http_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to persist interceptor: {}", e),
            ));
        }
    } else {
        // No store configured — still track the bus id.
        state.interceptor_ids.write().await.insert(bus_id.clone());
    }

    debug!(
        "Registered webhook interceptor {} for pattern {}",
        bus_id, req.pattern
    );

    Ok(Json(RegisterInterceptorResponse { id: bus_id }))
}

/// Remove a webhook-backed sync interceptor previously registered via
/// `POST /interceptors`. Returns 404 if the id is not a known
/// SyncInterceptor (cross-type deletion of an async webhook
/// subscription is refused — the prior implementation walked the bus
/// trie and accidentally destroyed the wrong kind, corrupting the
/// counter).
async fn remove_interceptor(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, HttpError> {
    // F4: atomic check-and-remove on the type-segregated set. Prevents
    // (a) double-decrement on idempotent DELETEs, (b) cross-type
    // deletion via the wrong endpoint.
    let was_present = state.interceptor_ids.write().await.remove(&id);
    if !was_present {
        return Err(http_error_with_code(
            StatusCode::NOT_FOUND,
            format!("interceptor {} not found", id),
            "INTERCEPTOR_NOT_FOUND",
        ));
    }

    if let Err(e) = state.bus.unsubscribe(&id).await {
        // Bus said no — put the id back into the set so a retry can
        // succeed. Don't decrement the counter.
        state.interceptor_ids.write().await.insert(id.clone());
        warn!("Interceptor unsubscribe failed for id {}: {}", id, e);
        return Err(http_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{}", e),
        ));
    }

    // Decrement the cap counter. Saturating sub guards against any edge
    // case where the counter is already 0 (e.g. recreate failed but
    // somehow left an entry in the set).
    let prev = state.interceptor_count.load(Ordering::Relaxed);
    if prev > 0 {
        state.interceptor_count.fetch_sub(1, Ordering::Relaxed);
    }

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
    refuse_if_unauth_state_changing(&state, "GET /events/:seq")?;

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
