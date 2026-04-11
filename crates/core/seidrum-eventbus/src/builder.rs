use crate::bus::{EventBus, EventBusImpl, SubscribeOpts, Subscription};
use crate::delivery::{ChannelRegistry, DeliveryChannel, RetryConfig, RetryTask, WebhookChannel};
use crate::dispatch::{EventFilter, Interceptor, SubscriptionInfo};
use crate::request_reply::RequestSubscription;
use crate::storage::compaction::CompactionTask;
use crate::storage::EventStore;
use crate::transport::{HttpServer, WebSocketServer};
use crate::EventBusError;
use async_trait::async_trait;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Wrapper that holds the bus and keeps the shutdown sender alive.
/// When this is dropped, the sender is dropped too, signaling shutdown.
struct OwnedBus {
    inner: Arc<dyn EventBus>,
    _shutdown: watch::Sender<bool>,
}

#[async_trait]
impl EventBus for OwnedBus {
    async fn publish(&self, subject: &str, payload: &[u8]) -> crate::Result<u64> {
        self.inner.publish(subject, payload).await
    }
    async fn subscribe(&self, pattern: &str, opts: SubscribeOpts) -> crate::Result<Subscription> {
        self.inner.subscribe(pattern, opts).await
    }
    async fn unsubscribe(&self, id: &str) -> crate::Result<()> {
        self.inner.unsubscribe(id).await
    }
    async fn request(
        &self,
        subject: &str,
        payload: &[u8],
        timeout: Duration,
    ) -> crate::Result<Vec<u8>> {
        self.inner.request(subject, payload, timeout).await
    }
    async fn serve(
        &self,
        subject: &str,
        priority: u32,
        timeout: Duration,
        filter: Option<EventFilter>,
    ) -> crate::Result<RequestSubscription> {
        self.inner.serve(subject, priority, timeout, filter).await
    }
    async fn intercept(
        &self,
        pattern: &str,
        priority: u32,
        interceptor: Arc<dyn Interceptor>,
        timeout: Option<Duration>,
    ) -> crate::Result<String> {
        self.inner
            .intercept(pattern, priority, interceptor, timeout)
            .await
    }
    async fn list_subscriptions(
        &self,
        filter: Option<&str>,
    ) -> crate::Result<Vec<SubscriptionInfo>> {
        self.inner.list_subscriptions(filter).await
    }
    async fn metrics(&self) -> crate::Result<crate::bus::BusMetrics> {
        self.inner.metrics().await
    }
    async fn get_event(&self, seq: u64) -> crate::Result<Option<crate::storage::StoredEvent>> {
        self.inner.get_event(seq).await
    }
    async fn register_channel_type(
        &self,
        channel_type: &str,
        provider: Arc<dyn DeliveryChannel>,
    ) -> crate::Result<()> {
        self.inner
            .register_channel_type(channel_type, provider)
            .await
    }
}

/// Builder for constructing an EventBus with configurable options.
pub struct EventBusBuilder {
    store: Option<Arc<dyn EventStore>>,
    compaction_interval: Duration,
    retention: Duration,
    ws_addr: Option<SocketAddr>,
    http_addr: Option<SocketAddr>,
    retry: Option<RetryConfig>,
    /// Set independently of `with_retry` so order doesn't matter.
    retry_poll_interval: Option<Duration>,
    /// Set independently of `with_retry` so order doesn't matter.
    retry_query_limit: Option<usize>,
    /// Optional user-supplied registry. If `None`, a fresh empty one is built.
    registry: Option<Arc<ChannelRegistry>>,
    /// Pending channel registrations queued via `register_channel`. Drained
    /// during `build_with_handles` against the resolved registry.
    pending_channels: Vec<(String, Arc<dyn DeliveryChannel>)>,
}

/// Default compaction interval: 1 hour.
const DEFAULT_COMPACTION_INTERVAL: Duration = Duration::from_secs(3600);
/// Default retention period for delivered events: 24 hours.
const DEFAULT_RETENTION: Duration = Duration::from_secs(86400);
/// Default poll interval for the retry task: 1 second.
const DEFAULT_RETRY_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Default query limit per retry poll cycle: 256 deliveries.
const DEFAULT_RETRY_QUERY_LIMIT: usize = 256;

/// Handles to background tasks spawned by the builder.
///
/// Callers can use these handles to detect if a server task panicked or exited
/// unexpectedly. Call [`BusHandles::shutdown`] to non-blockingly signal
/// shutdown, or [`BusHandles::shutdown_and_join`] (consuming `self`) to
/// signal **and** await all background tasks. Dropping the handles does
/// **not** cancel the tasks.
pub struct BusHandles {
    /// The constructed event bus.
    pub bus: Arc<dyn EventBus>,
    /// Handle to the compaction background task.
    pub compaction: JoinHandle<()>,
    /// Handle to the WebSocket server task, if configured.
    pub ws_server: Option<JoinHandle<()>>,
    /// Handle to the HTTP server task, if configured.
    pub http_server: Option<JoinHandle<()>>,
    /// Handle to the retry task, if `with_retry()` was called.
    pub retry_task: Option<JoinHandle<()>>,
    /// Send `true` to gracefully shut down all transport servers.
    shutdown_tx: watch::Sender<bool>,
}

impl BusHandles {
    /// Signal all background tasks to shut down. Non-blocking — does not
    /// wait for tasks to actually exit. Use [`Self::shutdown_and_join`] when
    /// you need ordered shutdown.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Signal shutdown and `await` every background task to actually exit.
    /// Consumes `self` because the joined `JoinHandle`s cannot be reused.
    ///
    /// Errors from individual `JoinHandle`s (e.g., a task panicked) are
    /// logged at warn level but do not propagate.
    pub async fn shutdown_and_join(self) {
        let _ = self.shutdown_tx.send(true);
        let mut tasks: Vec<(&'static str, JoinHandle<()>)> = vec![("compaction", self.compaction)];
        if let Some(h) = self.ws_server {
            tasks.push(("websocket", h));
        }
        if let Some(h) = self.http_server {
            tasks.push(("http", h));
        }
        if let Some(h) = self.retry_task {
            tasks.push(("retry", h));
        }
        for (name, handle) in tasks {
            if let Err(e) = handle.await {
                tracing::warn!(task = name, error = %e, "background task did not exit cleanly");
            }
        }
    }
}

impl EventBusBuilder {
    pub fn new() -> Self {
        Self {
            store: None,
            compaction_interval: DEFAULT_COMPACTION_INTERVAL,
            retention: DEFAULT_RETENTION,
            ws_addr: None,
            http_addr: None,
            retry: None,
            retry_poll_interval: None,
            retry_query_limit: None,
            registry: None,
            pending_channels: Vec::new(),
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

    /// Enable the background retry task with the given config.
    ///
    /// `retry.initial_backoff_ms` and `retry.max_backoff_ms` flow through
    /// to the dispatch engine and retry task — both use them when computing
    /// `next_retry` timestamps via [`crate::delivery::calculate_backoff`].
    /// `retry.max_attempts` is the cap before deliveries are dead-lettered.
    ///
    /// Use [`Self::with_retry_poll_interval`] (in any order) to override the
    /// default 1s poll interval, and [`Self::with_retry_query_limit`] to
    /// override the default 256 deliveries-per-cycle batch size.
    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = Some(retry);
        self
    }

    /// Override the retry task's polling interval. Order-independent —
    /// works whether called before or after [`Self::with_retry`].
    pub fn with_retry_poll_interval(mut self, interval: Duration) -> Self {
        self.retry_poll_interval = Some(interval);
        self
    }

    /// Override the per-cycle query limit (max deliveries fetched per poll).
    /// Order-independent.
    pub fn with_retry_query_limit(mut self, limit: usize) -> Self {
        self.retry_query_limit = Some(limit);
        self
    }

    /// Use a user-supplied [`ChannelRegistry`].
    ///
    /// The same registry is shared with the dispatch engine (for retry-time
    /// `Custom` channel lookups) and the HTTP transport (for webhook
    /// subscriptions). If not called, the builder constructs a fresh empty
    /// registry.
    pub fn with_channel_registry(mut self, registry: Arc<ChannelRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Register a custom delivery channel under the given type name.
    ///
    /// Channels registered here are looked up at retry time when a
    /// subscription uses `ChannelConfig::Custom { channel_type, .. }`.
    /// Registrations are deferred until `build_with_handles` so the order
    /// relative to `with_channel_registry` doesn't matter.
    pub fn register_channel(
        mut self,
        type_name: impl Into<String>,
        channel: Arc<dyn DeliveryChannel>,
    ) -> Self {
        self.pending_channels.push((type_name.into(), channel));
        self
    }

    /// Build the EventBus, returning only the bus (dropping task handles).
    ///
    /// Background tasks (compaction, transport servers) are spawned and will run
    /// for the lifetime of the tokio runtime. Use [`Self::build_with_handles`] if you
    /// need to monitor or await the background tasks.
    pub async fn build(self) -> crate::Result<Arc<dyn EventBus>> {
        let handles = self.build_with_handles().await?;
        let bus = handles.bus;
        // Wrap the bus with the shutdown sender so that:
        // - The sender stays alive as long as the bus Arc is alive
        // - Transport servers shut down when the last Arc is dropped
        Ok(Arc::new(OwnedBus {
            inner: bus,
            _shutdown: handles.shutdown_tx,
        }))
    }

    /// Build the EventBus and return handles to all spawned background tasks.
    ///
    /// This gives the caller the ability to detect panics or unexpected exits
    /// from the compaction task or transport servers. Call
    /// [`BusHandles::shutdown`] to gracefully stop transport servers.
    pub async fn build_with_handles(self) -> crate::Result<BusHandles> {
        let store = self
            .store
            .ok_or_else(|| EventBusError::Config("storage backend is required".to_string()))?;

        // Resolve registry: use the user-supplied one or create a fresh
        // empty registry. Either way, the same Arc is shared between the
        // dispatch engine (retry path) and the HTTP transport (webhook
        // subscriptions) and any user-registered Custom channels.
        let registry = self
            .registry
            .unwrap_or_else(|| Arc::new(ChannelRegistry::new()));

        // Drain pending channel registrations against the resolved registry.
        for (type_name, channel) in self.pending_channels {
            registry.register(&type_name, channel).await;
        }

        // Single shared WebhookChannel — used by both the dispatch engine
        // (for retry-time delivery) and the HTTP transport (for live
        // delivery from webhook subscriptions).
        let webhook_channel = WebhookChannel::new();

        // Resolve retry config: defaults applied if `with_retry` was not
        // called. Always Some so the engine can use it for first-failure
        // backoff regardless of whether the retry task is enabled.
        let retry_enabled = self.retry.is_some();
        let retry_config = Arc::new(self.retry.clone().unwrap_or_default());

        let mut engine = crate::dispatch::DispatchEngine::with_components(
            Arc::clone(&store),
            Arc::clone(&registry),
            Arc::clone(&webhook_channel),
            Arc::clone(&retry_config),
        );
        engine.set_retry_enabled(retry_enabled);
        let engine = Arc::new(engine);

        // Wrap the engine in EventBusImpl. The bus_impl uses the same engine
        // we'll hand to RetryTask, so retry-time subscription lookups see
        // exactly the same trie state as live publish/subscribe.
        let bus_impl = EventBusImpl::from_engine(Arc::clone(&engine));
        let bus: Arc<dyn EventBus> = Arc::new(bus_impl);

        // Shared shutdown signal for all background tasks
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        // Start compaction task
        let compaction_task = CompactionTask::new(
            Arc::clone(&store),
            self.compaction_interval,
            self.retention,
            shutdown_rx.clone(),
        );
        let compaction_handle = tokio::spawn(async move {
            compaction_task.run().await;
        });

        // Start retry task if configured.
        let retry_handle = if self.retry.is_some() {
            let poll_interval = self
                .retry_poll_interval
                .unwrap_or(DEFAULT_RETRY_POLL_INTERVAL);
            let query_limit = self.retry_query_limit.unwrap_or(DEFAULT_RETRY_QUERY_LIMIT);
            let task = RetryTask::new(
                Arc::clone(&store),
                Arc::clone(&engine),
                Arc::clone(&retry_config),
                poll_interval,
                query_limit,
                shutdown_rx.clone(),
            );
            Some(tokio::spawn(async move {
                task.run().await;
            }))
        } else {
            None
        };

        // Start WebSocket server if configured
        let ws_handle = if let Some(addr) = self.ws_addr {
            let bus_clone = Arc::clone(&bus);
            let rx = shutdown_rx.clone();
            Some(tokio::spawn(async move {
                let ws_server = WebSocketServer::new(bus_clone, rx);
                if let Err(e) = ws_server.start(addr).await {
                    tracing::error!("WebSocket server error: {}", e);
                }
            }))
        } else {
            None
        };

        // Start HTTP server if configured
        let http_handle = if let Some(addr) = self.http_addr {
            let bus_clone = Arc::clone(&bus);
            let registry_clone = Arc::clone(&registry);
            let webhook_clone = Arc::clone(&webhook_channel);
            let rx = shutdown_rx.clone();
            Some(tokio::spawn(async move {
                let http_server =
                    HttpServer::with_webhook_channel(bus_clone, registry_clone, webhook_clone, rx);
                if let Err(e) = http_server.start(addr).await {
                    tracing::error!("HTTP server error: {}", e);
                }
            }))
        } else {
            None
        };

        Ok(BusHandles {
            bus,
            compaction: compaction_handle,
            ws_server: ws_handle,
            http_server: http_handle,
            retry_task: retry_handle,
            shutdown_tx,
        })
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

    #[tokio::test]
    async fn test_builder_with_handles() {
        let store = Arc::new(InMemoryEventStore::new());
        let handles = EventBusBuilder::new()
            .storage(store)
            .build_with_handles()
            .await
            .unwrap();

        // Bus should be operational
        assert!(handles.bus.metrics().await.is_ok());
        // Compaction task should be running
        assert!(!handles.compaction.is_finished());
        // No transport servers configured
        assert!(handles.ws_server.is_none());
        assert!(handles.http_server.is_none());
    }

    #[tokio::test]
    async fn test_builder_shutdown() {
        let store = Arc::new(InMemoryEventStore::new());
        let handles = EventBusBuilder::new()
            .storage(store)
            .build_with_handles()
            .await
            .unwrap();

        // Shutdown should not panic even without transport servers
        handles.shutdown();
    }
}
