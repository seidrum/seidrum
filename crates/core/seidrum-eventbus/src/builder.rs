use crate::bus::{EventBus, EventBusImpl};
use crate::storage::compaction::CompactionTask;
use crate::storage::EventStore;
use crate::EventBusError;
use std::sync::Arc;
use std::time::Duration;

/// Builder for constructing an EventBus with configurable options.
pub struct EventBusBuilder {
    store: Option<Arc<dyn EventStore>>,
    compaction_interval: Duration,
    retention: Duration,
}

/// Default compaction interval: 1 hour.
const DEFAULT_COMPACTION_INTERVAL: Duration = Duration::from_secs(3600);
/// Default retention period for delivered events: 24 hours.
const DEFAULT_RETENTION: Duration = Duration::from_secs(86400);

impl EventBusBuilder {
    pub fn new() -> Self {
        Self {
            store: None,
            compaction_interval: DEFAULT_COMPACTION_INTERVAL,
            retention: DEFAULT_RETENTION,
        }
    }

    /// Set the event store backend.
    pub fn storage(mut self, store: Arc<dyn EventStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Set the compaction interval.
    pub fn compaction_interval(mut self, interval: Duration) -> Self {
        self.compaction_interval = interval;
        self
    }

    /// Set the retention duration for delivered events.
    pub fn retention(mut self, duration: Duration) -> Self {
        self.retention = duration;
        self
    }

    /// Build the EventBus.
    /// Note: The compaction task is spawned as a background task and runs for the lifetime
    /// of the bus. To stop it, drop the returned bus.
    pub async fn build(self) -> crate::Result<Arc<dyn EventBus>> {
        let store = self
            .store
            .ok_or_else(|| EventBusError::Config("storage backend is required".to_string()))?;

        let bus = Arc::new(EventBusImpl::new(Arc::clone(&store)));

        // Start compaction task using the configured interval
        let compaction_task =
            CompactionTask::new(Arc::clone(&store), self.compaction_interval, self.retention);
        // Spawn compaction task as a background task. It will continue until the bus is dropped.
        // Handle task panics by logging an error instead of silently crashing.
        tokio::spawn(async move {
            compaction_task.run().await;
        });

        Ok(bus)
    }
}

impl Default for EventBusBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::memory_store::InMemoryEventStore;

    #[tokio::test]
    async fn test_builder_minimal() {
        let store = Arc::new(InMemoryEventStore::new());
        let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

        // Verify the bus is operational — no subscriptions exist yet
        let subs = bus.list_subscriptions(None).await.unwrap();
        assert!(subs.is_empty());
    }

    #[tokio::test]
    async fn test_builder_with_options() {
        let store = Arc::new(InMemoryEventStore::new());
        let bus = EventBusBuilder::new()
            .storage(store)
            .compaction_interval(Duration::from_secs(300))
            .retention(Duration::from_secs(3600))
            .build()
            .await
            .unwrap();

        assert!(bus.metrics().await.is_ok());
    }
}
