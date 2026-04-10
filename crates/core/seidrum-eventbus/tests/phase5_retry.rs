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
    let store = Arc::new(InMemoryEventStore::new());
    let engine = Arc::new(DispatchEngine::with_registry(
        Arc::clone(&store) as Arc<dyn EventStore>,
        registry,
    ));

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let retry_task = RetryTask::new(
        Arc::clone(&store) as Arc<dyn EventStore>,
        Arc::clone(&engine),
        max_attempts,
        Duration::from_millis(50),
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
    store
        .record_delivery(
            seq,
            sub_id,
            DeliveryStatus::Failed,
            Some("initial failure".to_string()),
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
    let flaky = FlakyChannel::new(u32::MAX); // always fails
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
