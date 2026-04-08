use crate::delivery::ChannelConfig;
use crate::dispatch::{DispatchEngine, SubscriptionInfo, SubscriptionMode};
use crate::storage::EventStore;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Metrics from the bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusMetrics {
    pub events_published: u64,
    pub events_delivered: u64,
    pub events_pending_retry: u64,
    pub subscription_count: u64,
}

/// A subscription returned from subscribe().
#[derive(Debug)]
pub struct Subscription {
    pub id: String,
    pub rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
}

/// Options for subscribing.
#[derive(Debug, Clone)]
pub struct SubscribeOpts {
    pub priority: u32,
    pub mode: SubscriptionMode,
    pub channel: ChannelConfig,
    pub timeout: std::time::Duration,
}

/// The EventBus trait is the public API for the entire event bus.
#[async_trait]
pub trait EventBus: Send + Sync + 'static {
    /// Publish an event to a subject. The event is persisted durably
    /// before dispatch begins. Returns the assigned sequence number.
    async fn publish(&self, subject: &str, payload: &[u8]) -> crate::Result<u64>;

    /// Subscribe to a subject pattern with the given options.
    /// Returns a Subscription handle for receiving events.
    async fn subscribe(&self, pattern: &str, opts: SubscribeOpts) -> crate::Result<Subscription>;

    /// Remove a subscription by ID.
    async fn unsubscribe(&self, id: &str) -> crate::Result<()>;

    /// List active subscriptions, optionally filtered by pattern.
    async fn list_subscriptions(
        &self,
        filter: Option<&str>,
    ) -> crate::Result<Vec<SubscriptionInfo>>;

    /// Get bus metrics.
    async fn metrics(&self) -> crate::Result<BusMetrics>;
}

/// The default EventBus implementation.
pub struct EventBusImpl {
    engine: Arc<DispatchEngine>,
}

impl EventBusImpl {
    pub fn new(store: Arc<dyn EventStore>) -> Self {
        Self {
            engine: Arc::new(DispatchEngine::new(store)),
        }
    }
}

#[async_trait]
impl EventBus for EventBusImpl {
    async fn publish(&self, subject: &str, payload: &[u8]) -> crate::Result<u64> {
        self.engine.publish(subject, payload).await
    }

    async fn subscribe(&self, pattern: &str, opts: SubscribeOpts) -> crate::Result<Subscription> {
        let (id, rx) = self
            .engine
            .subscribe(pattern, opts.priority, opts.mode, opts.timeout)
            .await?;
        Ok(Subscription { id, rx })
    }

    async fn unsubscribe(&self, id: &str) -> crate::Result<()> {
        self.engine.unsubscribe(id).await
    }

    async fn list_subscriptions(
        &self,
        filter: Option<&str>,
    ) -> crate::Result<Vec<SubscriptionInfo>> {
        self.engine.list_subscriptions(filter).await
    }

    async fn metrics(&self) -> crate::Result<BusMetrics> {
        // TODO: Phase 2 will track detailed publish/deliver counters
        Ok(BusMetrics {
            events_published: 0,
            events_delivered: 0,
            events_pending_retry: 0,
            subscription_count: self.list_subscriptions(None).await?.len() as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::memory_store::InMemoryEventStore;

    #[tokio::test]
    async fn test_publish_deliver() {
        let store = Arc::new(InMemoryEventStore::new());
        let bus = EventBusImpl::new(store);

        let opts = SubscribeOpts {
            priority: 10,
            mode: SubscriptionMode::Async,
            channel: ChannelConfig::InProcess,
            timeout: std::time::Duration::from_secs(5),
        };
        let mut sub = bus.subscribe("test.subject", opts).await.unwrap();

        bus.publish("test.subject", b"hello").await.unwrap();

        let msg = tokio::time::timeout(std::time::Duration::from_secs(1), sub.rx.recv())
            .await
            .unwrap();
        assert_eq!(msg, Some(b"hello".to_vec()));
    }

    #[tokio::test]
    async fn test_multi_subscriber() {
        let store = Arc::new(InMemoryEventStore::new());
        let bus = EventBusImpl::new(store);

        let opts = SubscribeOpts {
            priority: 10,
            mode: SubscriptionMode::Async,
            channel: ChannelConfig::InProcess,
            timeout: std::time::Duration::from_secs(5),
        };

        let mut sub1 = bus.subscribe("test.subject", opts.clone()).await.unwrap();
        let mut sub2 = bus.subscribe("test.subject", opts.clone()).await.unwrap();

        bus.publish("test.subject", b"message").await.unwrap();

        let msg1 = tokio::time::timeout(std::time::Duration::from_secs(1), sub1.rx.recv())
            .await
            .unwrap();
        let msg2 = tokio::time::timeout(std::time::Duration::from_secs(1), sub2.rx.recv())
            .await
            .unwrap();

        assert_eq!(msg1, Some(b"message".to_vec()));
        assert_eq!(msg2, Some(b"message".to_vec()));
    }

    #[tokio::test]
    async fn test_builder_minimal() {
        let store = Arc::new(InMemoryEventStore::new());
        let bus = EventBusImpl::new(store);
        let metrics = bus.metrics().await.unwrap();
        assert_eq!(metrics.subscription_count, 0);
    }

    #[tokio::test]
    async fn test_list_subscriptions() {
        let store = Arc::new(InMemoryEventStore::new());
        let bus = EventBusImpl::new(store);

        let opts = SubscribeOpts {
            priority: 10,
            mode: SubscriptionMode::Async,
            channel: ChannelConfig::InProcess,
            timeout: std::time::Duration::from_secs(5),
        };

        let _sub1 = bus.subscribe("test.a", opts.clone()).await.unwrap();
        let _sub2 = bus.subscribe("test.b", opts.clone()).await.unwrap();

        let all = bus.list_subscriptions(None).await.unwrap();
        assert_eq!(all.len(), 2);

        let filtered = bus.list_subscriptions(Some("test.a")).await.unwrap();
        assert_eq!(filtered.len(), 1);
    }
}
