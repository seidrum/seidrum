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

#[cfg(test)]
pub mod test_utils {
    use crate::storage::memory_store::InMemoryEventStore;
    use crate::{EventBus, EventBusBuilder};
    use std::sync::Arc;

    /// Create an in-memory bus for testing.
    pub async fn test_bus() -> Arc<dyn EventBus> {
        let store = Arc::new(InMemoryEventStore::new());
        EventBusBuilder::new().storage(store).build().await.unwrap()
    }
}
