use crate::bus::{EventBus, EventBusImpl};
use crate::delivery::ChannelRegistry;
use crate::storage::compaction::CompactionTask;
use crate::storage::EventStore;
use crate::transport::{HttpServer, WebSocketServer};
use crate::EventBusError;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// Builder for constructing an EventBus with configurable options.
pub struct EventBusBuilder {
    store: Option<Arc<dyn EventStore>>,
    compaction_interval: Duration,
    retention: Duration,
    ws_addr: Option<SocketAddr>,
    http_addr: Option<SocketAddr>,
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
            ws_addr: None,
            http_addr: None,
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

    /// Enable WebSocket server on the given address.
    pub fn with_websocket(mut self, addr: SocketAddr) -> Self {
        self.ws_addr = Some(addr);
        self
    }

    /// Enable HTTP server on the given address.
    pub fn with_http(mut self, addr: SocketAddr) -> Self {
        self.http_addr = Some(addr);
        self
    }

    /// Build the EventBus.
    /// Note: The compaction task is spawned as a background task and runs for the lifetime
    /// of the bus. To stop it, drop the returned bus.
    /// Also starts WebSocket and HTTP servers if configured.
    pub async fn build(self) -> crate::Result<Arc<dyn EventBus>> {
        let store = self
            .store
            .ok_or_else(|| EventBusError::Config("storage backend is required".to_string()))?;

        let bus = Arc::new(EventBusImpl::new(Arc::clone(&store)));

        // Start compaction task using the configured interval
        let compaction_task =
            CompactionTask::new(Arc::clone(&store), self.compaction_interval, self.retention);
        // Spawn compaction task as a background task. It will continue until the bus is dropped.
        tokio::spawn(async move {
            compaction_task.run().await;
        });

        // Start WebSocket server if configured
        if let Some(addr) = self.ws_addr {
            let bus_clone = Arc::clone(&bus);
            tokio::spawn(async move {
                let ws_server = WebSocketServer::new(bus_clone);
                if let Err(e) = ws_server.start(addr).await {
                    tracing::error!("WebSocket server error: {}", e);
                }
            });
        }

        // Start HTTP server if configured
        if let Some(addr) = self.http_addr {
            let bus_clone = Arc::clone(&bus);
            let registry = Arc::new(ChannelRegistry::new());
            tokio::spawn(async move {
                let http_server = HttpServer::new(bus_clone, registry);
                if let Err(e) = http_server.start(addr).await {
                    tracing::error!("HTTP server error: {}", e);
                }
            });
        }

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
