//! seidrum-eventbus: A purpose-built event bus for Seidrum.
//!
//! This crate provides a high-performance, durable event bus with support for
//! in-process delivery, subject-based routing, and crash recovery.

pub mod builder;
pub mod bus;
pub mod delivery;
pub mod dispatch;
pub mod request_reply;
pub mod storage;

pub use builder::EventBusBuilder;
pub use bus::{BusMetrics, EventBus, EventBusImpl, SubscribeOpts, Subscription};
pub use delivery::{ChannelConfig, DeliveryChannel, DeliveryReceipt};
pub use dispatch::{EventFilter, InterceptResult, Interceptor, SubscriptionInfo, SubscriptionMode};
pub use request_reply::{DispatchedEvent, Replier, RequestMessage, RequestSubscription};
pub use storage::{EventStatus, EventStore, StoredEvent};

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
