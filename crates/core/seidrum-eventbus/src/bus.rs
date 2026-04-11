use crate::delivery::{ChannelConfig, DeliveryChannel};
use crate::dispatch::{
    DispatchEngine, EventFilter, Interceptor, SubscriptionInfo, SubscriptionMode,
};
use crate::request_reply::RequestSubscription;
use crate::storage::{EventStore, StoredEvent};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

/// Aggregate runtime metrics for an [`EventBus`] instance.
///
/// Currently only `subscription_count` is populated; the other counters
/// are placeholders for a future metrics overhaul.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusMetrics {
    pub events_published: u64,
    pub events_delivered: u64,
    pub events_pending_retry: u64,
    pub subscription_count: u64,
}

/// Handle returned from [`EventBus::subscribe`].
///
/// Contains the subscription `id` (used to call [`EventBus::unsubscribe`])
/// and an `rx` channel for receiving dispatched events. Dropping the
/// `Subscription` does **not** unsubscribe — call `unsubscribe(id)` to
/// release the entry from the bus.
#[derive(Debug)]
pub struct Subscription {
    pub id: String,
    pub rx: tokio::sync::mpsc::Receiver<crate::request_reply::DispatchedEvent>,
}

/// Options passed to [`EventBus::subscribe`].
///
/// - `priority`: lower values run first in the sync chain. Async subscribers
///   are unordered relative to each other but always after the sync chain.
/// - `mode`: [`SubscriptionMode::Sync`] (interceptor-style, can modify the
///   payload) or [`SubscriptionMode::Async`] (typical pub/sub).
/// - `channel`: tagging metadata describing how the subscription is wired
///   (e.g. in-process, webhook, websocket). Currently used by the transport
///   layer; the dispatch engine routes to the in-process mpsc channel
///   regardless.
/// - `timeout`: per-interceptor timeout for sync subscribers; ignored for
///   pure async subscribers.
/// - `filter`: optional [`EventFilter`] to narrow the subscription.
#[derive(Debug, Clone)]
pub struct SubscribeOpts {
    pub priority: u32,
    pub mode: SubscriptionMode,
    pub channel: ChannelConfig,
    pub timeout: Duration,
    pub filter: Option<EventFilter>,
}

/// The `EventBus` trait is the public API for the entire event bus.
///
/// Implementations must be `Send + Sync + 'static` so the bus can be
/// shared across tasks via `Arc<dyn EventBus>`.
#[async_trait]
pub trait EventBus: Send + Sync + 'static {
    /// Publish an event to a subject.
    ///
    /// The event is persisted durably (write-ahead) before dispatch begins.
    /// Returns the assigned sequence number on success.
    ///
    /// Subjects beginning with `_reply.` are reserved for internal
    /// request/reply routing and are rejected with
    /// [`crate::EventBusError::InvalidSubject`].
    async fn publish(&self, subject: &str, payload: &[u8]) -> crate::Result<u64>;

    /// Subscribe to a subject pattern.
    ///
    /// `pattern` may contain `*` (single-token wildcard) or `>`
    /// (multi-token tail wildcard, must be the last token). Returns a
    /// [`Subscription`] handle whose `rx` channel receives matching events.
    async fn subscribe(&self, pattern: &str, opts: SubscribeOpts) -> crate::Result<Subscription>;

    /// Remove a subscription by ID.
    async fn unsubscribe(&self, id: &str) -> crate::Result<()>;

    /// Publish a request and wait for a reply.
    ///
    /// Generates a unique `_reply.{ulid}` reply subject, publishes the
    /// request with `reply_subject` set, and waits for a response.
    /// Returns the reply payload, [`crate::EventBusError::RequestTimeout`]
    /// if no reply arrives within `timeout`, or
    /// [`crate::EventBusError::ReplyChannelClosed`] if the reply
    /// subscription is dropped before a reply arrives.
    async fn request(
        &self,
        subject: &str,
        payload: &[u8],
        timeout: Duration,
    ) -> crate::Result<Vec<u8>>;

    /// Register a request handler on a subject pattern.
    ///
    /// Similar to [`subscribe`](Self::subscribe), but the returned
    /// [`RequestSubscription`] yields `(RequestMessage, Replier)` pairs.
    /// The handler calls [`crate::Replier::reply`] to send a response.
    ///
    /// The handler always uses [`SubscriptionMode::Async`] internally with
    /// in-process delivery. Use `priority` to control ordering relative to
    /// other subscribers on the same subject. The `subject` parameter
    /// supports the same wildcards as `subscribe`.
    async fn serve(
        &self,
        subject: &str,
        priority: u32,
        timeout: Duration,
        filter: Option<EventFilter>,
    ) -> crate::Result<RequestSubscription>;

    /// Register a sync interceptor for a subject pattern.
    ///
    /// Returns the subscription ID. The interceptor runs in priority order
    /// during the sync chain and can `Pass`, `Modify`, or `Drop` events.
    /// `timeout` defaults to 5 seconds if not specified.
    async fn intercept(
        &self,
        pattern: &str,
        priority: u32,
        interceptor: Arc<dyn Interceptor>,
        timeout: Option<Duration>,
    ) -> crate::Result<String>;

    /// List active subscriptions, optionally filtered by exact subject pattern.
    ///
    /// When `filter` is `Some`, only subscriptions whose `subject_pattern`
    /// equals the filter exactly are returned.
    async fn list_subscriptions(
        &self,
        filter: Option<&str>,
    ) -> crate::Result<Vec<SubscriptionInfo>>;

    /// Get aggregate bus metrics.
    async fn metrics(&self) -> crate::Result<BusMetrics>;

    /// Fetch a single persisted event by its sequence number.
    /// Returns `Ok(None)` if no event with that seq exists (or has been
    /// compacted away).
    ///
    /// Used by the HTTP transport's `GET /events/:seq` endpoint and by
    /// debugging tools that need to inspect specific events without
    /// scanning by status.
    async fn get_event(&self, seq: u64) -> crate::Result<Option<StoredEvent>>;

    /// Register a custom delivery channel type at runtime.
    ///
    /// Subscriptions that use `ChannelConfig::Custom { channel_type, .. }`
    /// resolve their delivery channel via the bus's [`crate::delivery::ChannelRegistry`]
    /// at retry time. This method is the runtime equivalent of
    /// [`crate::EventBusBuilder::register_channel`] — it lets remote
    /// plugins (or in-process code added after the bus is built) register
    /// new channel types without rebuilding the bus.
    ///
    /// If a channel was already registered under the same `channel_type`,
    /// it is silently replaced; the registry logs a warning.
    async fn register_channel_type(
        &self,
        channel_type: &str,
        provider: Arc<dyn DeliveryChannel>,
    ) -> crate::Result<()>;
}

/// The default EventBus implementation.
pub struct EventBusImpl {
    engine: Arc<DispatchEngine>,
}

impl EventBusImpl {
    /// Construct an `EventBusImpl` from an event store with default
    /// retry configuration and an empty channel registry.
    ///
    /// **Note:** the engine wired up by this constructor cannot share its
    /// `ChannelRegistry` with anything else (e.g., the HTTP transport's
    /// webhook subscriptions or the retry task's `Custom` channel lookups).
    /// For production use, prefer constructing via [`crate::EventBusBuilder`]
    /// which wires up a single shared registry across the engine, transports,
    /// and retry task.
    pub fn new(store: Arc<dyn EventStore>) -> Self {
        Self {
            engine: Arc::new(DispatchEngine::new(store)),
        }
    }

    /// Construct an `EventBusImpl` that shares a [`crate::delivery::ChannelRegistry`]
    /// with the dispatch engine. The registry is used by the retry task for
    /// looking up `ChannelConfig::Custom` delivery channels.
    pub fn with_registry(
        store: Arc<dyn EventStore>,
        registry: Arc<crate::delivery::ChannelRegistry>,
    ) -> Self {
        Self {
            engine: Arc::new(DispatchEngine::with_registry(store, registry)),
        }
    }

    /// Wrap an already-constructed [`DispatchEngine`] in an `EventBusImpl`.
    /// Used by [`crate::EventBusBuilder`] so the engine, retry task, and
    /// transports all share the same instance.
    pub fn from_engine(engine: Arc<DispatchEngine>) -> Self {
        Self { engine }
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
        priority: u32,
        timeout: Duration,
        filter: Option<EventFilter>,
    ) -> crate::Result<RequestSubscription> {
        self.engine.serve(subject, priority, timeout, filter).await
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
        use std::sync::atomic::Ordering;
        // Only report `events_pending_retry` if a retry task is actually
        // running. Otherwise the count is meaningless — there's no worker
        // to drain it. A bus without `with_retry()` reports 0 here so
        // dashboards don't surface a permanent backlog.
        let pending_retry = if self.engine.retry_enabled {
            match self
                .engine
                .store
                .count_retryable(self.engine.retry_config.max_attempts)
                .await
            {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(error = %e, "metrics: count_retryable failed");
                    0
                }
            }
        } else {
            0
        };

        Ok(BusMetrics {
            events_published: self.engine.events_published.load(Ordering::Relaxed),
            events_delivered: self.engine.events_delivered.load(Ordering::Relaxed),
            events_pending_retry: pending_retry,
            subscription_count: self.list_subscriptions(None).await?.len() as u64,
        })
    }

    async fn get_event(&self, seq: u64) -> crate::Result<Option<StoredEvent>> {
        self.engine
            .store
            .get(seq)
            .await
            .map_err(crate::EventBusError::Storage)
    }

    async fn register_channel_type(
        &self,
        channel_type: &str,
        provider: Arc<dyn DeliveryChannel>,
    ) -> crate::Result<()> {
        self.engine
            .channel_registry
            .register(channel_type, provider)
            .await;
        Ok(())
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
        assert_eq!(metrics.events_published, 0);
        assert_eq!(metrics.events_delivered, 0);
    }

    #[tokio::test]
    async fn test_metrics_publish_deliver_counters() {
        let store = Arc::new(InMemoryEventStore::new());
        let bus = EventBusImpl::new(store);

        let opts = SubscribeOpts {
            priority: 10,
            mode: SubscriptionMode::Async,
            channel: ChannelConfig::InProcess,
            timeout: Duration::from_secs(5),
            filter: None,
        };
        let mut sub = bus.subscribe("test.metrics", opts).await.unwrap();

        bus.publish("test.metrics", b"a").await.unwrap();
        bus.publish("test.metrics", b"b").await.unwrap();
        bus.publish("test.metrics", b"c").await.unwrap();

        // Drain so the channel doesn't fill
        for _ in 0..3 {
            let _ = tokio::time::timeout(Duration::from_millis(100), sub.rx.recv()).await;
        }

        let metrics = bus.metrics().await.unwrap();
        assert_eq!(metrics.events_published, 3);
        assert_eq!(metrics.events_delivered, 3);
        assert_eq!(metrics.subscription_count, 1);
    }

    #[tokio::test]
    async fn test_get_event_round_trip() {
        let store = Arc::new(InMemoryEventStore::new());
        let bus = EventBusImpl::new(store);

        let seq = bus.publish("test.get", b"hello get").await.unwrap();

        let event = bus.get_event(seq).await.unwrap();
        assert!(event.is_some());
        let event = event.unwrap();
        assert_eq!(event.seq, seq);
        assert_eq!(event.subject, "test.get");
        assert_eq!(event.payload, b"hello get");
    }

    #[tokio::test]
    async fn test_get_event_not_found() {
        let store = Arc::new(InMemoryEventStore::new());
        let bus = EventBusImpl::new(store);

        let event = bus.get_event(9999).await.unwrap();
        assert!(event.is_none());
    }

    #[tokio::test]
    async fn test_register_channel_type_via_bus() {
        use crate::delivery::{
            DeliveryChannel as DC, DeliveryError, DeliveryReceipt, DeliveryResult,
        };
        use async_trait::async_trait;

        struct DummyChannel;

        #[async_trait]
        impl DC for DummyChannel {
            async fn deliver(
                &self,
                _event: &[u8],
                _subject: &str,
                _config: &ChannelConfig,
            ) -> DeliveryResult<DeliveryReceipt> {
                Err(DeliveryError::NotReady)
            }
            async fn cleanup(&self, _config: &ChannelConfig) -> DeliveryResult<()> {
                Ok(())
            }
            async fn is_healthy(&self, _config: &ChannelConfig) -> bool {
                true
            }
        }

        let store = Arc::new(InMemoryEventStore::new());
        let bus = EventBusImpl::new(store);

        // Should succeed without error.
        let result = bus
            .register_channel_type("dummy", Arc::new(DummyChannel) as Arc<dyn DC>)
            .await;
        assert!(result.is_ok());
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

    #[tokio::test]
    async fn test_publish_reply_subject_rejected() {
        let store = Arc::new(InMemoryEventStore::new());
        let bus = EventBusImpl::new(store);

        // Direct publish to _reply.* must be rejected — these subjects are
        // reserved for internal request/reply routing.
        let result = bus.publish("_reply.foo", b"data").await;
        assert!(matches!(
            result,
            Err(crate::EventBusError::InvalidSubject(_))
        ));
    }
}
