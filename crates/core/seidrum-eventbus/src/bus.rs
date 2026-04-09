use crate::delivery::ChannelConfig;
use crate::dispatch::{
    DispatchEngine, EventFilter, Interceptor, SubscriptionInfo, SubscriptionMode,
};
use crate::request_reply::RequestSubscription;
use crate::storage::EventStore;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

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
    pub rx: tokio::sync::mpsc::Receiver<crate::request_reply::DispatchedEvent>,
}

/// Options for subscribing.
#[derive(Debug, Clone)]
pub struct SubscribeOpts {
    pub priority: u32,
    pub mode: SubscriptionMode,
    pub channel: ChannelConfig,
    pub timeout: Duration,
    pub filter: Option<EventFilter>,
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

    /// Publish a request and wait for a reply.
    ///
    /// Generates a unique reply subject, publishes the request to the target
    /// subject with the reply_subject set, and waits for a response.
    /// Returns the reply payload or `Err(RequestTimeout)` if no reply arrives
    /// within the timeout duration.
    async fn request(
        &self,
        subject: &str,
        payload: &[u8],
        timeout: Duration,
    ) -> crate::Result<Vec<u8>>;

    /// Register a request handler on a subject.
    ///
    /// Similar to subscribe(), but the returned RequestSubscription yields
    /// (RequestMessage, Replier) pairs. The handler can call replier.reply()
    /// to send a response to the requester.
    async fn serve(&self, subject: &str, opts: SubscribeOpts)
        -> crate::Result<RequestSubscription>;

    /// Register a sync interceptor for a subject pattern.
    /// Returns the subscription ID. The interceptor runs in priority order
    /// during the sync chain and can modify or drop events.
    async fn intercept(
        &self,
        pattern: &str,
        priority: u32,
        interceptor: Arc<dyn Interceptor>,
        timeout: Option<Duration>,
    ) -> crate::Result<String>;

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
        // Reject publication to reserved internal subjects.
        // _reply.* subjects are used for request/reply routing and must not be
        // published to directly — only Replier::reply() may do so internally.
        if subject.starts_with("_reply.") {
            return Err(crate::EventBusError::InvalidSubject(
                "subjects starting with '_reply.' are reserved for internal request/reply routing"
                    .to_string(),
            ));
        }
        self.engine.publish(subject, payload).await
    }

    async fn subscribe(&self, pattern: &str, opts: SubscribeOpts) -> crate::Result<Subscription> {
        let (id, rx) = self
            .engine
            .subscribe(
                pattern,
                opts.priority,
                opts.mode,
                opts.channel,
                opts.timeout,
                opts.filter,
            )
            .await?;
        Ok(Subscription { id, rx })
    }

    async fn unsubscribe(&self, id: &str) -> crate::Result<()> {
        self.engine.unsubscribe(id).await
    }

    async fn request(
        &self,
        subject: &str,
        payload: &[u8],
        timeout: Duration,
    ) -> crate::Result<Vec<u8>> {
        self.engine.request(subject, payload, timeout).await
    }

    async fn serve(
        &self,
        subject: &str,
        opts: SubscribeOpts,
    ) -> crate::Result<RequestSubscription> {
        self.engine
            .serve(subject, opts.priority, opts.timeout, opts.filter)
            .await
    }

    async fn intercept(
        &self,
        pattern: &str,
        priority: u32,
        interceptor: Arc<dyn Interceptor>,
        timeout: Option<Duration>,
    ) -> crate::Result<String> {
        self.engine
            .intercept(pattern, priority, interceptor, timeout)
            .await
    }

    async fn list_subscriptions(
        &self,
        filter: Option<&str>,
    ) -> crate::Result<Vec<SubscriptionInfo>> {
        self.engine.list_subscriptions(filter).await
    }

    async fn metrics(&self) -> crate::Result<BusMetrics> {
        // TODO: Phase 3+ will track detailed publish/deliver counters
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
    use crate::dispatch::InterceptResult;
    use crate::storage::memory_store::InMemoryEventStore;

    #[tokio::test]
    async fn test_publish_deliver() {
        let store = Arc::new(InMemoryEventStore::new());
        let bus = EventBusImpl::new(store);

        let opts = SubscribeOpts {
            priority: 10,
            mode: SubscriptionMode::Async,
            channel: ChannelConfig::InProcess,
            timeout: Duration::from_secs(5),
            filter: None,
        };
        let mut sub = bus.subscribe("test.subject", opts).await.unwrap();

        bus.publish("test.subject", b"hello").await.unwrap();

        let msg = tokio::time::timeout(Duration::from_secs(1), sub.rx.recv())
            .await
            .unwrap();
        assert_eq!(msg.map(|e| e.payload), Some(b"hello".to_vec()));
    }

    #[tokio::test]
    async fn test_multi_subscriber() {
        let store = Arc::new(InMemoryEventStore::new());
        let bus = EventBusImpl::new(store);

        let opts = SubscribeOpts {
            priority: 10,
            mode: SubscriptionMode::Async,
            channel: ChannelConfig::InProcess,
            timeout: Duration::from_secs(5),
            filter: None,
        };

        let mut sub1 = bus.subscribe("test.subject", opts.clone()).await.unwrap();
        let mut sub2 = bus.subscribe("test.subject", opts.clone()).await.unwrap();

        bus.publish("test.subject", b"message").await.unwrap();

        let msg1 = tokio::time::timeout(Duration::from_secs(1), sub1.rx.recv())
            .await
            .unwrap();
        let msg2 = tokio::time::timeout(Duration::from_secs(1), sub2.rx.recv())
            .await
            .unwrap();

        assert_eq!(msg1.map(|e| e.payload), Some(b"message".to_vec()));
        assert_eq!(msg2.map(|e| e.payload), Some(b"message".to_vec()));
    }

    #[tokio::test]
    async fn test_metrics() {
        let store = Arc::new(InMemoryEventStore::new());
        let bus = EventBusImpl::new(store);
        let metrics = bus.metrics().await.unwrap();
        assert_eq!(metrics.subscription_count, 0);
    }

    #[tokio::test]
    async fn test_intercept_via_bus() {
        struct PrefixInterceptor;

        #[async_trait]
        impl Interceptor for PrefixInterceptor {
            async fn intercept(&self, _subject: &str, payload: &mut Vec<u8>) -> InterceptResult {
                let mut new = b"[intercepted] ".to_vec();
                new.extend_from_slice(payload);
                *payload = new;
                InterceptResult::Modified
            }
        }

        let store = Arc::new(InMemoryEventStore::new());
        let bus = EventBusImpl::new(store);

        bus.intercept("test", 5, Arc::new(PrefixInterceptor), None)
            .await
            .unwrap();

        let opts = SubscribeOpts {
            priority: 10,
            mode: SubscriptionMode::Async,
            channel: ChannelConfig::InProcess,
            timeout: Duration::from_secs(5),
            filter: None,
        };
        let mut sub = bus.subscribe("test", opts).await.unwrap();

        bus.publish("test", b"hello").await.unwrap();

        let msg = tokio::time::timeout(Duration::from_secs(1), sub.rx.recv())
            .await
            .unwrap();
        assert_eq!(
            msg.map(|e| e.payload),
            Some(b"[intercepted] hello".to_vec())
        );
    }
}
