//! seidrum-eventbus: A purpose-built event bus for Seidrum.
//!
//! This crate provides a high-performance, durable event bus with support for
//! in-process delivery, subject-based routing, and crash recovery.

pub mod bus;
pub mod builder;
pub mod delivery;
pub mod dispatch;
pub mod storage;

pub use bus::{BusMetrics, EventBus, EventBusImpl, SubscribeOpts, Subscription};
pub use builder::EventBusBuilder;
pub use delivery::{ChannelConfig, DeliveryChannel, DeliveryReceipt};
pub use dispatch::{SubscriptionInfo, SubscriptionMode};
pub use storage::{EventStore, EventStatus, StoredEvent};

/// Result type for eventbus operations.
pub type Result<T> = std::result::Result<T, String>;

#[cfg(test)]
pub mod test_utils {
    use crate::{EventBus, EventBusBuilder};
    use crate::storage::memory_store::InMemoryEventStore;
    use std::sync::Arc;

    /// Create an in-memory bus for testing.
    pub async fn test_bus() -> Arc<dyn EventBus> {
        let store = Arc::new(InMemoryEventStore::new());
        EventBusBuilder::new()
            .storage(store)
            .build()
            .await
            .unwrap()
    }
}
