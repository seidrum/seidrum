//! seidrum-eventbus: a purpose-built event bus for Seidrum.
//!
//! `seidrum-eventbus` is a durable, in-process event bus with subject-based
//! routing, wildcard pattern matching, sync/async delivery modes, request/reply
//! semantics, and optional WebSocket/HTTP transports for remote clients.
//!
//! # Features
//!
//! - **Durable storage**: Events are persisted via the [`EventStore`] trait
//!   before dispatch (write-ahead). Backends include
//!   [`storage::memory_store::InMemoryEventStore`] for tests and
//!   [`storage::redb_store::RedbEventStore`] for production.
//! - **Subject routing**: NATS-style subjects (`channel.telegram.inbound`) with
//!   `*` (single-token) and `>` (multi-token) wildcards.
//! - **Sync interceptors**: Modify or drop events before delivery to async
//!   subscribers, with timeout enforcement.
//! - **Event filters**: JSON path-based predicates (`FieldEquals`,
//!   `FieldContains`, `All`/`Any` combinators) to narrow subscriptions.
//! - **Request/reply**: Built on top of pub/sub via dynamically-generated
//!   `_reply.{ulid}` reply subjects.
//! - **Remote transports**: Optional [`transport::WebSocketServer`] and
//!   [`transport::HttpServer`] for cross-process clients.
//! - **Crash recovery**: Pending events on disk are rediscovered on restart
//!   via [`EventStore::query_by_status`].
//!
//! # Quick start
//!
//! ```no_run
//! use seidrum_eventbus::{EventBusBuilder, SubscribeOpts, SubscriptionMode, ChannelConfig};
//! use seidrum_eventbus::storage::memory_store::InMemoryEventStore;
//! use std::sync::Arc;
//! use std::time::Duration;
//!
//! # async fn example() -> seidrum_eventbus::Result<()> {
//! let store = Arc::new(InMemoryEventStore::new());
//! let bus = EventBusBuilder::new().storage(store).build().await?;
//!
//! let opts = SubscribeOpts {
//!     priority: 10,
//!     mode: SubscriptionMode::Async,
//!     channel: ChannelConfig::InProcess,
//!     timeout: Duration::from_secs(5),
//!     filter: None,
//! };
//! let mut sub = bus.subscribe("channel.>", opts).await?;
//!
//! bus.publish("channel.telegram.inbound", b"hello").await?;
//!
//! if let Some(event) = sub.rx.recv().await {
//!     assert_eq!(event.payload, b"hello");
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Architecture
//!
//! Publishing an event walks a 6-stage pipeline:
//!
//! 1. **PERSIST** — write the event to the [`EventStore`] (write-ahead).
//! 2. **RESOLVE** — look up matching subscriptions in the trie-based
//!    [`dispatch::subject_index::SubjectIndex`].
//! 3. **FILTER** — apply each subscription's [`EventFilter`] to the original
//!    payload.
//! 4. **SYNC CHAIN** — run sync [`Interceptor`]s sequentially in priority
//!    order. Interceptors can `Pass`, `Modify`, or `Drop` the event.
//! 5. **ASYNC FAN-OUT** — deliver the (possibly mutated) payload to all
//!    async subscribers.
//! 6. **FINALIZE** — record the final [`EventStatus`] (`Delivered`,
//!    `PartiallyDelivered`, etc).
//!
//! See [`dispatch::engine`] for full details.
//!
//! # Reserved subjects
//!
//! Subjects beginning with `_reply.` are reserved for internal request/reply
//! routing. Direct calls to [`EventBus::publish`] with such subjects return
//! [`EventBusError::InvalidSubject`]. Reply events are not persisted to the
//! store.

pub mod builder;
pub mod bus;
pub mod delivery;
pub mod dispatch;
pub mod request_reply;
pub mod storage;
pub mod transport;

pub use builder::{BusHandles, EventBusBuilder};
pub use bus::{BusMetrics, EventBus, EventBusImpl, SubscribeOpts, Subscription};
pub use delivery::{
    ChannelConfig, ChannelRegistry, DeliveryChannel, DeliveryError, DeliveryReceipt,
    DeliveryResult, WebSocketChannel, WebhookChannel,
};
pub use dispatch::{EventFilter, InterceptResult, Interceptor, SubscriptionInfo, SubscriptionMode};
pub use request_reply::{DispatchedEvent, Replier, RequestMessage, RequestSubscription};
pub use storage::{EventStatus, EventStore, StoredEvent};
pub use transport::{HttpAuthenticator, HttpServer, NoHttpAuth, WebSocketServer};

/// Errors that can occur in the event bus.
#[derive(Debug, thiserror::Error)]
pub enum EventBusError {
    /// The storage backend returned an error.
    #[error("storage error: {0}")]
    Storage(#[from] storage::StorageError),

    /// A delivery channel returned an error.
    #[error("delivery error: {0}")]
    Delivery(#[from] delivery::DeliveryError),

    /// Invalid subject pattern.
    #[error("invalid subject: {0}")]
    InvalidSubject(String),

    /// Payload size exceeded the maximum allowed.
    #[error("payload too large: {0}")]
    PayloadTooLarge(String),

    /// Configuration or builder error.
    #[error("configuration error: {0}")]
    Config(String),

    /// A request timed out while waiting for a reply.
    #[error("request timed out")]
    RequestTimeout,

    /// The reply channel was closed before a reply was received.
    #[error("reply channel closed")]
    ReplyChannelClosed,

    /// An internal error.
    #[error("internal error: {0}")]
    Internal(String),
}

/// Result type for eventbus operations.
pub type Result<T> = std::result::Result<T, EventBusError>;

/// Test utilities for the event bus.
///
/// Reusable helpers for integration tests and downstream crates that need
/// to spin up a bus, capture interceptor calls, or mock a webhook receiver.
///
/// **Feature-gated:** enable the `test-utils` feature to use this module.
/// The crate's own integration tests in `tests/` enable it automatically
/// via `required-features` in `Cargo.toml`. Downstream crates that want
/// the helpers must opt in:
/// ```toml
/// [dev-dependencies]
/// seidrum-eventbus = { version = "*", features = ["test-utils"] }
/// ```
/// Production binaries do not pay the cost of axum/reqwest pulled in by
/// the mock webhook server.
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils {
    use crate::dispatch::{InterceptResult, Interceptor};
    use crate::request_reply::DispatchedEvent;
    use crate::storage::memory_store::InMemoryEventStore;
    use crate::storage::EventStore;
    use crate::{BusHandles, EventBus, EventBusBuilder};
    use async_trait::async_trait;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{mpsc, Mutex};

    /// Create an in-memory bus for testing.
    pub async fn test_bus() -> Arc<dyn EventBus> {
        test_bus_with_storage(Arc::new(InMemoryEventStore::new())).await
    }

    /// Create a bus backed by a caller-supplied storage backend.
    /// Useful for restart-recovery tests where the same store needs to
    /// outlive a bus.
    pub async fn test_bus_with_storage(store: Arc<dyn EventStore>) -> Arc<dyn EventBus> {
        EventBusBuilder::new().storage(store).build().await.unwrap()
    }

    /// Bus + transport endpoints, for end-to-end transport tests.
    ///
    /// Returns the bus handle and the resolved listening addresses for the
    /// HTTP and WebSocket servers (each on an ephemeral port chosen by the
    /// OS at startup time). The caller is responsible for awaiting
    /// readiness via [`wait_for_http_ready`] before issuing requests.
    pub struct BusWithTransports {
        pub handles: BusHandles,
        pub http_addr: SocketAddr,
        pub ws_addr: SocketAddr,
    }

    /// Create a bus with both HTTP and WebSocket transports listening on
    /// ephemeral ports. Uses an in-memory store. Returns a [`BusWithTransports`]
    /// containing the handles and the resolved addresses.
    ///
    /// Tests run with [`crate::delivery::WebhookUrlPolicy::Permissive`] so
    /// loopback URLs (used by [`MockWebhookServer`]) are accepted.
    pub async fn test_bus_with_transports() -> BusWithTransports {
        let http_addr = pick_ephemeral_addr();
        let ws_addr = pick_ephemeral_addr();
        let store = Arc::new(InMemoryEventStore::new());
        let handles = EventBusBuilder::new()
            .storage(store)
            .with_http(http_addr)
            .with_websocket(ws_addr)
            .with_webhook_url_policy(crate::delivery::WebhookUrlPolicy::Permissive)
            .unsafe_allow_http_dev_mode()
            .build_with_handles()
            .await
            .unwrap();
        BusWithTransports {
            handles,
            http_addr,
            ws_addr,
        }
    }

    /// Reserve an ephemeral port by binding then immediately releasing.
    /// The port is then handed to the actual server, which will bind it
    /// before anyone else races to claim it on a quiet test machine.
    pub fn pick_ephemeral_addr() -> SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        addr
    }

    /// Poll an HTTP server's `/health` endpoint until it responds, with a
    /// 2-second budget. Panics on timeout. Use immediately after starting
    /// a bus that has `with_http(...)` configured to avoid races between
    /// the test issuing requests and the server completing its bind.
    pub async fn wait_for_http_ready(addr: SocketAddr) {
        let client = reqwest::Client::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if tokio::time::Instant::now() >= deadline {
                panic!("HTTP server at {} did not become ready in time", addr);
            }
            if let Ok(resp) = client
                .get(format!("http://{}/health", addr))
                .timeout(Duration::from_millis(200))
                .send()
                .await
            {
                if resp.status().is_success() {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Poll a WebSocket server until a connection succeeds, with a
    /// 2-second budget. Panics on timeout. The WS protocol has no
    /// equivalent of `/health`, so this opens and immediately drops a
    /// throwaway connection — the only signal that the listener is up.
    /// Use after starting a bus configured with `with_websocket(...)`.
    pub async fn wait_for_ws_ready(addr: SocketAddr) {
        let url = format!("ws://{}", addr);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if tokio::time::Instant::now() >= deadline {
                panic!("WebSocket server at {} did not become ready in time", addr);
            }
            match tokio::time::timeout(
                Duration::from_millis(200),
                tokio_tungstenite::connect_async(&url),
            )
            .await
            {
                Ok(Ok((stream, _))) => {
                    drop(stream);
                    return;
                }
                _ => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
    }

    /// One captured interceptor invocation: the subject the event was
    /// published on and the payload as the interceptor saw it.
    #[derive(Debug, Clone)]
    pub struct RecordedEvent {
        pub subject: String,
        pub payload: Vec<u8>,
    }

    /// An [`Interceptor`] that records every event it sees and never
    /// modifies the payload. Useful for asserting that interceptors fire
    /// in the expected order or that filtering/wildcards behave as
    /// specified.
    pub struct RecordingInterceptor {
        events: Arc<Mutex<Vec<RecordedEvent>>>,
    }

    impl Default for RecordingInterceptor {
        fn default() -> Self {
            Self::new()
        }
    }

    impl RecordingInterceptor {
        pub fn new() -> Self {
            Self {
                events: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// Snapshot of all events captured so far.
        pub async fn events(&self) -> Vec<RecordedEvent> {
            self.events.lock().await.clone()
        }

        /// Number of events captured so far.
        pub async fn count(&self) -> usize {
            self.events.lock().await.len()
        }
    }

    #[async_trait]
    impl Interceptor for RecordingInterceptor {
        async fn intercept(&self, subject: &str, payload: &mut Vec<u8>) -> InterceptResult {
            self.events.lock().await.push(RecordedEvent {
                subject: subject.to_string(),
                payload: payload.clone(),
            });
            InterceptResult::Pass
        }
    }

    /// Publish an event and wait up to `timeout` for it to be received on
    /// `rx`. Returns the received event or `None` on timeout.
    pub async fn publish_and_wait(
        bus: &Arc<dyn EventBus>,
        subject: &str,
        payload: &[u8],
        rx: &mut mpsc::Receiver<DispatchedEvent>,
        timeout: Duration,
    ) -> Option<DispatchedEvent> {
        bus.publish(subject, payload).await.ok()?;
        tokio::time::timeout(timeout, rx.recv()).await.ok().flatten()
    }

    /// Drain up to `count` events from `rx` with a per-event timeout.
    /// Returns whatever it managed to collect — caller asserts the length.
    pub async fn collect_events(
        rx: &mut mpsc::Receiver<DispatchedEvent>,
        count: usize,
        per_event_timeout: Duration,
    ) -> Vec<DispatchedEvent> {
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            match tokio::time::timeout(per_event_timeout, rx.recv()).await {
                Ok(Some(ev)) => out.push(ev),
                _ => break,
            }
        }
        out
    }

    /// A mock webhook server that captures every POST it receives.
    ///
    /// Boots an axum server on an ephemeral port and stores each delivery
    /// in an internal `Vec`. Tests can use [`Self::url_for`] to construct
    /// the URL to subscribe against, and [`Self::received`] to inspect
    /// captured deliveries afterwards.
    pub struct MockWebhookServer {
        pub addr: SocketAddr,
        received: Arc<Mutex<Vec<MockWebhookDelivery>>>,
        shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
        handle: Option<tokio::task::JoinHandle<()>>,
    }

    /// One captured webhook delivery: the path it hit and the raw body.
    #[derive(Debug, Clone)]
    pub struct MockWebhookDelivery {
        pub path: String,
        pub body: Vec<u8>,
        pub headers: std::collections::HashMap<String, String>,
    }

    impl MockWebhookServer {
        /// Start a mock webhook server on an ephemeral port.
        pub async fn start() -> Self {
            use axum::{
                extract::{Path, State},
                http::HeaderMap,
                routing::post,
                Router,
            };

            let received: Arc<Mutex<Vec<MockWebhookDelivery>>> = Arc::new(Mutex::new(Vec::new()));
            let received_clone = Arc::clone(&received);

            async fn handler(
                State(store): State<Arc<Mutex<Vec<MockWebhookDelivery>>>>,
                Path(path): Path<String>,
                headers: HeaderMap,
                body: axum::body::Bytes,
            ) -> axum::http::StatusCode {
                let mut hdrs = std::collections::HashMap::new();
                for (k, v) in headers.iter() {
                    if let Ok(s) = v.to_str() {
                        hdrs.insert(k.to_string(), s.to_string());
                    }
                }
                store.lock().await.push(MockWebhookDelivery {
                    path,
                    body: body.to_vec(),
                    headers: hdrs,
                });
                axum::http::StatusCode::OK
            }

            let app = Router::new()
                .route("/{*path}", post(handler))
                .with_state(received_clone);

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
            let handle = tokio::spawn(async move {
                let _ = axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                    })
                    .await;
            });

            // Wait briefly for the server to be ready (bind already happened
            // synchronously via TcpListener::bind, so this is just a small
            // task-yield to let the spawn schedule).
            tokio::task::yield_now().await;

            Self {
                addr,
                received,
                shutdown_tx: Some(shutdown_tx),
                handle: Some(handle),
            }
        }

        /// Build a webhook URL pointing at this mock server with the given
        /// path suffix (no leading slash needed).
        pub fn url_for(&self, path: &str) -> String {
            let p = path.trim_start_matches('/');
            format!("http://{}/{}", self.addr, p)
        }

        /// Snapshot of all deliveries received so far.
        pub async fn received(&self) -> Vec<MockWebhookDelivery> {
            self.received.lock().await.clone()
        }

        /// Number of deliveries received so far.
        pub async fn count(&self) -> usize {
            self.received.lock().await.len()
        }

        /// Wait until at least `n` deliveries have arrived, or `timeout`
        /// elapses. Returns the snapshot at the time the wait completes
        /// (which may have fewer than `n` if it timed out).
        pub async fn wait_for(&self, n: usize, timeout: Duration) -> Vec<MockWebhookDelivery> {
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                {
                    let guard = self.received.lock().await;
                    if guard.len() >= n {
                        return guard.clone();
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    return self.received.lock().await.clone();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        /// Gracefully stop the server. Called automatically on drop, but
        /// tests may invoke it explicitly to assert orderly shutdown.
        pub async fn shutdown(mut self) {
            if let Some(tx) = self.shutdown_tx.take() {
                let _ = tx.send(());
            }
            if let Some(h) = self.handle.take() {
                let _ = h.await;
            }
        }
    }

    impl Drop for MockWebhookServer {
        fn drop(&mut self) {
            if let Some(tx) = self.shutdown_tx.take() {
                let _ = tx.send(());
            }
            // The spawned task will be detached if not explicitly joined.
        }
    }
}
