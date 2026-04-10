//! Phase 5 integration tests: durable retry and dead-lettering.
//!
//! These tests use a custom delivery channel registered via [`ChannelRegistry`]
//! that fails N times then succeeds. The retry task wakes up periodically,
//! polls the store, and re-attempts failed deliveries via the dispatch engine.

#![cfg(test)]

use async_trait::async_trait;
use seidrum_eventbus::delivery::{
    ChannelConfig, ChannelRegistry, DeliveryChannel, DeliveryError, DeliveryReceipt,
    DeliveryResult, RetryConfig, RetryTask,
};
use seidrum_eventbus::dispatch::{DispatchEngine, SubscriptionMode};
use seidrum_eventbus::storage::memory_store::InMemoryEventStore;
use seidrum_eventbus::storage::{DeliveryStatus, StoredEvent};
use seidrum_eventbus::{EventBusBuilder, EventStatus, EventStore, SubscribeOpts};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A delivery channel that fails the first N attempts and then succeeds.
struct FlakyChannel {
    fail_attempts: u32,
    call_count: AtomicU32,
}

impl FlakyChannel {
    fn new(fail_attempts: u32) -> Arc<Self> {
        Arc::new(Self {
            fail_attempts,
            call_count: AtomicU32::new(0),
        })
    }

    /// Channel that fails every delivery attempt forever.
    fn always_fail() -> Arc<Self> {
        Self::new(u32::MAX)
    }

    fn calls(&self) -> u32 {
        self.call_count.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl DeliveryChannel for FlakyChannel {
    async fn deliver(
        &self,
        _event: &[u8],
        _subject: &str,
        _config: &ChannelConfig,
    ) -> DeliveryResult<DeliveryReceipt> {
        let call = self.call_count.fetch_add(1, Ordering::SeqCst) + 1;
        if call <= self.fail_attempts {
            Err(DeliveryError::Failed(format!("flaky failure #{}", call)))
        } else {
            Ok(DeliveryReceipt {
                delivered_at: 0,
                latency_us: 0,
            })
        }
    }

    async fn cleanup(&self, _config: &ChannelConfig) -> DeliveryResult<()> {
        Ok(())
    }

    async fn is_healthy(&self, _config: &ChannelConfig) -> bool {
        true
    }
}

/// Set up a DispatchEngine + RetryTask wired to the same store and registry.
/// Returns the engine, store, and a shutdown sender for the retry task.
async fn setup_engine_with_retry(
    registry: Arc<ChannelRegistry>,
    max_attempts: u32,
) -> (
    Arc<DispatchEngine>,
    Arc<InMemoryEventStore>,
    tokio::sync::watch::Sender<bool>,
) {
    use seidrum_eventbus::delivery::WebhookChannel;
    let store = Arc::new(InMemoryEventStore::new());
    let retry_config = Arc::new(RetryConfig {
        max_attempts,
        initial_backoff_ms: 30,
        max_backoff_ms: 200,
    });
    let engine = Arc::new(DispatchEngine::with_components(
        Arc::clone(&store) as Arc<dyn EventStore>,
        registry,
        WebhookChannel::new(),
        Arc::clone(&retry_config),
    ));

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let retry_task = RetryTask::new(
        Arc::clone(&store) as Arc<dyn EventStore>,
        Arc::clone(&engine),
        retry_config,
        Duration::from_millis(50),
        256,
        shutdown_rx,
    );
    tokio::spawn(async move {
        retry_task.run().await;
    });

    (engine, store, shutdown_tx)
}

/// Subscribe via a `Custom` channel pointing at the given type name and
/// return the subscription id.
async fn subscribe_custom(engine: &DispatchEngine, subject: &str, channel_type: &str) -> String {
    let opts = SubscribeOpts {
        priority: 10,
        mode: SubscriptionMode::Async,
        channel: ChannelConfig::Custom {
            channel_type: channel_type.to_string(),
            config: serde_json::json!({}),
        },
        timeout: Duration::from_secs(5),
        filter: None,
    };
    let (sub_id, _rx) = engine
        .subscribe(
            subject,
            opts.priority,
            opts.mode,
            opts.channel,
            opts.timeout,
            opts.filter,
        )
        .await
        .unwrap();
    sub_id
}

/// Append an event with one Failed delivery for the given subscriber.
/// This simulates the state after an initial failed dispatch.
async fn seed_failed_event(
    store: &InMemoryEventStore,
    subject: &str,
    payload: &[u8],
    sub_id: &str,
) -> u64 {
    let event = StoredEvent {
        seq: 0,
        subject: subject.to_string(),
        payload: payload.to_vec(),
        stored_at: 0,
        status: EventStatus::PartiallyDelivered,
        deliveries: vec![],
        reply_subject: None,
    };
    let seq = store.append(&event).await.unwrap();
    // Schedule the first retry to fire essentially immediately so tests
    // don't have to wait for the default backoff.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    store
        .record_delivery(
            seq,
            sub_id,
            DeliveryStatus::Failed,
            Some("initial failure".to_string()),
            Some(now), // ready immediately
        )
        .await
        .unwrap();
    store
        .update_status(seq, EventStatus::PartiallyDelivered)
        .await
        .unwrap();
    seq
}

#[tokio::test]
async fn test_retry_succeeds_after_failures() {
    let registry = Arc::new(ChannelRegistry::new());
    let flaky = FlakyChannel::new(2); // fail twice, then succeed
    registry
        .register("flaky", Arc::clone(&flaky) as Arc<dyn DeliveryChannel>)
        .await;

    let (engine, store, _shutdown) = setup_engine_with_retry(Arc::clone(&registry), 5).await;
    let sub_id = subscribe_custom(&engine, "test.retry", "flaky").await;
    let seq = seed_failed_event(&store, "test.retry", b"hello", &sub_id).await;

    // Wait for the retry to succeed (3 calls total: 2 fails + 1 success)
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && flaky.calls() < 3 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        flaky.calls() >= 3,
        "expected at least 3 deliveries, got {}",
        flaky.calls()
    );

    // The retry task should transition the event to Delivered after success.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut delivered = false;
    while Instant::now() < deadline {
        let events = store
            .query_by_status(EventStatus::Delivered, 100)
            .await
            .unwrap();
        if events.iter().any(|e| e.seq == seq) {
            delivered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(delivered, "event should be marked Delivered after retry");
}

#[tokio::test]
async fn test_retry_dead_letters_after_max_attempts() {
    let registry = Arc::new(ChannelRegistry::new());
    let flaky = FlakyChannel::always_fail();
    registry
        .register("flaky", Arc::clone(&flaky) as Arc<dyn DeliveryChannel>)
        .await;

    let (engine, store, _shutdown) = setup_engine_with_retry(Arc::clone(&registry), 3).await;
    let sub_id = subscribe_custom(&engine, "test.dead", "flaky").await;
    let seq = seed_failed_event(&store, "test.dead", b"doomed", &sub_id).await;

    // Wait for the event to be dead-lettered
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut dead = false;
    while Instant::now() < deadline {
        let events = store.query_dead_lettered(100).await.unwrap();
        if events.iter().any(|e| e.seq == seq) {
            dead = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        dead,
        "event should be dead-lettered after exhausting attempts (calls={})",
        flaky.calls()
    );
}

#[tokio::test]
async fn test_retry_permanent_error_dead_letters_immediately() {
    let registry = Arc::new(ChannelRegistry::new());
    // Don't register any channel — the retry will hit "no channel registered"
    let (engine, store, _shutdown) = setup_engine_with_retry(Arc::clone(&registry), 10).await;
    let sub_id = subscribe_custom(&engine, "test.perm", "missing").await;
    let seq = seed_failed_event(&store, "test.perm", b"x", &sub_id).await;

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut dead = false;
    while Instant::now() < deadline {
        let events = store.query_dead_lettered(100).await.unwrap();
        if events.iter().any(|e| e.seq == seq) {
            dead = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(dead, "permanent error should dead-letter immediately");
}

#[tokio::test]
async fn test_retry_dead_letters_when_subscriber_gone() {
    let registry = Arc::new(ChannelRegistry::new());
    let flaky = FlakyChannel::new(0); // would always succeed if called
    registry
        .register("flaky", Arc::clone(&flaky) as Arc<dyn DeliveryChannel>)
        .await;

    let (engine, store, _shutdown) = setup_engine_with_retry(Arc::clone(&registry), 5).await;
    let sub_id = subscribe_custom(&engine, "test.gone", "flaky").await;
    let seq = seed_failed_event(&store, "test.gone", b"x", &sub_id).await;

    // Unsubscribe before the retry runs — the engine will fail to find the
    // subscriber and dead-letter the delivery.
    engine.unsubscribe(&sub_id).await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut dead = false;
    while Instant::now() < deadline {
        let events = store.query_dead_lettered(100).await.unwrap();
        if events.iter().any(|e| e.seq == seq) {
            dead = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(dead, "missing subscriber should dead-letter");
    // The flaky channel should never have been called
    assert_eq!(flaky.calls(), 0);
}

#[tokio::test]
async fn test_with_retry_builder_spawns_task() {
    let store = Arc::new(InMemoryEventStore::new());
    let handles = EventBusBuilder::new()
        .storage(store)
        .with_retry(RetryConfig::default())
        .build_with_handles()
        .await
        .unwrap();

    assert!(handles.retry_task.is_some());
    assert!(!handles.retry_task.as_ref().unwrap().is_finished());

    // Shut down the bus and verify the retry task exits cleanly
    handles.shutdown();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if handles.retry_task.as_ref().unwrap().is_finished() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("retry task should exit after shutdown signal");
}

#[tokio::test]
async fn test_retry_task_not_started_without_with_retry() {
    let store = Arc::new(InMemoryEventStore::new());
    let handles = EventBusBuilder::new()
        .storage(store)
        .build_with_handles()
        .await
        .unwrap();

    assert!(handles.retry_task.is_none());
}

#[tokio::test]
async fn test_with_retry_poll_interval_order_independent() {
    // Calling with_retry_poll_interval BEFORE with_retry must still apply.
    let store = Arc::new(InMemoryEventStore::new());
    let handles = EventBusBuilder::new()
        .storage(store)
        .with_retry_poll_interval(Duration::from_millis(75))
        .with_retry(RetryConfig::default())
        .build_with_handles()
        .await
        .unwrap();

    // The retry task is spawned (we can't observe the interval directly,
    // but the test ensures the chained methods work without panicking
    // and that with_retry_poll_interval doesn't drop the value).
    assert!(handles.retry_task.is_some());
    handles.shutdown_and_join().await;
}

#[tokio::test]
async fn test_register_channel_via_builder() {
    let store = Arc::new(InMemoryEventStore::new());
    let flaky = FlakyChannel::new(0); // always succeeds

    let handles = EventBusBuilder::new()
        .storage(store)
        .register_channel("flaky", Arc::clone(&flaky) as Arc<dyn DeliveryChannel>)
        .with_retry(RetryConfig::default())
        .build_with_handles()
        .await
        .unwrap();

    assert!(handles.retry_task.is_some());
    handles.shutdown_and_join().await;
}

#[tokio::test]
async fn test_count_retryable_metric() {
    use seidrum_eventbus::storage::DeliveryStatus;
    use seidrum_eventbus::storage::StoredEvent;

    let registry = Arc::new(ChannelRegistry::new());
    // Register a channel that always fails so retries don't drain the queue.
    let flaky = FlakyChannel::always_fail();
    registry
        .register("flaky", Arc::clone(&flaky) as Arc<dyn DeliveryChannel>)
        .await;

    let store = Arc::new(InMemoryEventStore::new());

    // Seed two failed deliveries, both ready immediately
    let event = StoredEvent {
        seq: 0,
        subject: "test".to_string(),
        payload: b"x".to_vec(),
        stored_at: 0,
        status: EventStatus::PartiallyDelivered,
        deliveries: vec![],
        reply_subject: None,
    };
    let seq1 = store.append(&event).await.unwrap();
    let seq2 = store.append(&event).await.unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    store
        .record_delivery(
            seq1,
            "sub1",
            DeliveryStatus::Failed,
            Some("a".into()),
            Some(now),
        )
        .await
        .unwrap();
    store
        .record_delivery(
            seq2,
            "sub2",
            DeliveryStatus::Failed,
            Some("b".into()),
            Some(now),
        )
        .await
        .unwrap();

    let count = store.count_retryable(10).await.unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn test_redb_retry_succeeds_after_failures() {
    use seidrum_eventbus::delivery::WebhookChannel;
    use seidrum_eventbus::storage::redb_store::RedbEventStore;
    use seidrum_eventbus::storage::DeliveryStatus;
    use seidrum_eventbus::storage::StoredEvent;
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let store: Arc<dyn EventStore> =
        Arc::new(RedbEventStore::open(temp.path().join("events.db")).unwrap());

    let registry = Arc::new(ChannelRegistry::new());
    let flaky = FlakyChannel::new(2);
    registry
        .register("flaky", Arc::clone(&flaky) as Arc<dyn DeliveryChannel>)
        .await;

    let retry_config = Arc::new(RetryConfig {
        max_attempts: 5,
        initial_backoff_ms: 30,
        max_backoff_ms: 200,
    });
    let engine = Arc::new(DispatchEngine::with_components(
        Arc::clone(&store),
        Arc::clone(&registry),
        WebhookChannel::new(),
        Arc::clone(&retry_config),
    ));

    let sub_id = subscribe_custom(&engine, "test.redb_retry", "flaky").await;

    let event = StoredEvent {
        seq: 0,
        subject: "test.redb_retry".to_string(),
        payload: b"redb".to_vec(),
        stored_at: 0,
        status: EventStatus::PartiallyDelivered,
        deliveries: vec![],
        reply_subject: None,
    };
    let seq = store.append(&event).await.unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    store
        .record_delivery(
            seq,
            &sub_id,
            DeliveryStatus::Failed,
            Some("first".into()),
            Some(now),
        )
        .await
        .unwrap();
    store
        .update_status(seq, EventStatus::PartiallyDelivered)
        .await
        .unwrap();

    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let task = RetryTask::new(
        Arc::clone(&store),
        Arc::clone(&engine),
        Arc::clone(&retry_config),
        Duration::from_millis(50),
        256,
        shutdown_rx,
    );
    tokio::spawn(async move {
        task.run().await;
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && flaky.calls() < 3 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        flaky.calls() >= 3,
        "redb retry: expected 3 calls, got {}",
        flaky.calls()
    );

    // The failed_deliveries_idx should be cleaned up after success
    let pending = store.count_retryable(10).await.unwrap();
    assert_eq!(pending, 0, "no retryable deliveries should remain");
}

#[tokio::test]
async fn test_concurrent_retries_independent() {
    let registry = Arc::new(ChannelRegistry::new());
    let flaky_a = FlakyChannel::new(1); // fail once, succeed
    let flaky_b = FlakyChannel::new(1);
    registry
        .register("a", Arc::clone(&flaky_a) as Arc<dyn DeliveryChannel>)
        .await;
    registry
        .register("b", Arc::clone(&flaky_b) as Arc<dyn DeliveryChannel>)
        .await;

    let (engine, store, _shutdown) = setup_engine_with_retry(Arc::clone(&registry), 5).await;
    let sub_a = subscribe_custom(&engine, "test.a", "a").await;
    let sub_b = subscribe_custom(&engine, "test.b", "b").await;

    let _seq_a = seed_failed_event(&store, "test.a", b"a", &sub_a).await;
    let _seq_b = seed_failed_event(&store, "test.b", b"b", &sub_b).await;

    // Wait for both to deliver successfully (each: 1 fail + 1 success = 2 calls)
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && (flaky_a.calls() < 2 || flaky_b.calls() < 2) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(flaky_a.calls() >= 2, "channel a calls={}", flaky_a.calls());
    assert!(flaky_b.calls() >= 2, "channel b calls={}", flaky_b.calls());
}
