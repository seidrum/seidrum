use crate::bus::{EventBus, EventBusImpl, SubscribeOpts, Subscription};
use crate::delivery::{ChannelRegistry, RetryConfig, RetryTask};
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
}

/// Builder for constructing an EventBus with configurable options.
pub struct EventBusBuilder {
    store: Option<Arc<dyn EventStore>>,
    compaction_interval: Duration,
    retention: Duration,
    ws_addr: Option<SocketAddr>,
    http_addr: Option<SocketAddr>,
    retry: Option<RetryTaskConfig>,
}

/// Default compaction interval: 1 hour.
const DEFAULT_COMPACTION_INTERVAL: Duration = Duration::from_secs(3600);
/// Default retention period for delivered events: 24 hours.
const DEFAULT_RETENTION: Duration = Duration::from_secs(86400);
/// Default poll interval for the retry task: 1 second.
const DEFAULT_RETRY_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Internal config for the retry task — combines retry parameters with poll interval.
#[derive(Debug, Clone)]
struct RetryTaskConfig {
    retry: RetryConfig,
    poll_interval: Duration,
}

/// Handles to background tasks spawned by the builder.
///
/// Callers can use these handles to detect if a server task panicked or exited
/// unexpectedly. Call [`BusHandles::shutdown`] to gracefully stop transport
/// servers. Dropping the handles does **not** cancel the tasks.
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
    /// Signal all transport servers to shut down gracefully.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
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
    /// Polls the store every second by default; use [`Self::with_retry_poll_interval`]
    /// to change the interval. Failed deliveries are retried via the dispatch
    /// engine, which looks up the live subscription and routes through the
    /// configured channel. Deliveries whose attempt count reaches
    /// `retry.max_attempts` (or hit a permanent error) are dead-lettered.
    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = Some(RetryTaskConfig {
            retry,
            poll_interval: DEFAULT_RETRY_POLL_INTERVAL,
        });
        self
    }

    /// Override the retry task's polling interval. Has no effect unless
    /// [`Self::with_retry`] was also called.
    pub fn with_retry_poll_interval(mut self, interval: Duration) -> Self {
        if let Some(cfg) = self.retry.as_mut() {
            cfg.poll_interval = interval;
        }
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

        // Shared registry between the dispatch engine (for retry channel
        // lookups) and the HTTP transport (for webhook subscriptions).
        let registry = Arc::new(ChannelRegistry::new());

        let bus_impl = EventBusImpl::with_registry(Arc::clone(&store), Arc::clone(&registry));
        let engine = bus_impl.engine();
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
        // Backoff parameters in `cfg.retry` (initial_backoff_ms, max_backoff_ms)
        // are currently informational; the store hardcodes 100ms × 2^attempts
        // capped at 30s. Future work: thread these into the store layer.
        let retry_handle = if let Some(cfg) = self.retry {
            let task = RetryTask::new(
                Arc::clone(&store),
                Arc::clone(&engine),
                cfg.retry.max_attempts,
                cfg.poll_interval,
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
            let rx = shutdown_rx.clone();
            Some(tokio::spawn(async move {
                let http_server = HttpServer::new(bus_clone, registry_clone, rx);
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
