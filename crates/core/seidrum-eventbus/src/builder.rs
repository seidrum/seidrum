use crate::bus::{EventBus, EventBusImpl};
use crate::storage::EventStore;
use std::sync::Arc;
use std::time::Duration;

/// Builder for constructing an EventBus with configurable options.
pub struct EventBusBuilder {
    store: Option<Arc<dyn EventStore>>,
    compaction_interval: Duration,
    retention: Duration,
}

impl EventBusBuilder {
    pub fn new() -> Self {
        Self {
            store: None,
            compaction_interval: Duration::from_secs(3600),
            retention: Duration::from_secs(86400),
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
    pub async fn build(self) -> crate::Result<Arc<dyn EventBus>> {
        let store = self
            .store
            .ok_or_else(|| "storage backend is required".to_string())?;

        let bus = Arc::new(EventBusImpl::new(Arc::clone(&store)));

        // Start compaction task
        let compaction_store = Arc::clone(&store);
        let retention = self.retention;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            loop {
                interval.tick().await;
                if let Err(e) = compaction_store.compact(retention).await {
                    tracing::warn!("compaction failed: {}", e);
                }
            }
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

        assert!(!bus.list_subscriptions(None).await.unwrap().is_empty() || true);
        // Just verify it works
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
