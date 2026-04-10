use seidrum_eventbus::storage::memory_store::InMemoryEventStore;
use seidrum_eventbus::{
    ChannelConfig, EventBusBuilder, EventFilter, EventStore, InterceptResult, Interceptor,
    SubscribeOpts, SubscriptionMode,
};
use std::sync::Arc;
use std::time::Duration;

fn default_opts() -> SubscribeOpts {
    SubscribeOpts {
        priority: 10,
        mode: SubscriptionMode::Async,
        channel: ChannelConfig::InProcess,
        timeout: Duration::from_secs(5),
        filter: None,
    }
}

// --- Phase 1 tests (preserved) ---

#[tokio::test]
async fn test_publish_deliver_end_to_end() {
    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    let mut sub = bus.subscribe("test.subject", default_opts()).await.unwrap();

    let seq = bus.publish("test.subject", b"hello").await.unwrap();
    assert!(seq > 0);

    let msg = tokio::time::timeout(Duration::from_secs(1), sub.rx.recv())
        .await
        .unwrap();
    assert_eq!(msg.map(|e| e.payload), Some(b"hello".to_vec()));
}

#[tokio::test]
async fn test_crash_recovery_full() {
    use seidrum_eventbus::storage::redb_store::RedbEventStore;
    use seidrum_eventbus::storage::{DeliveryStatus, EventStatus, StoredEvent};
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let path = temp.path().join("events.db");

    {
        let store = RedbEventStore::open(&path).unwrap();
        let event1 = StoredEvent {
            seq: 0,
            subject: "test.subject".to_string(),
            payload: b"event1".to_vec(),
            stored_at: 0,
            status: EventStatus::Pending,
            deliveries: vec![],
            reply_subject: None,
        };
        let event2 = StoredEvent {
            seq: 0,
            subject: "test.subject".to_string(),
            payload: b"event2".to_vec(),
            stored_at: 0,
            status: EventStatus::Pending,
            deliveries: vec![],
            reply_subject: None,
        };
        store.append(&event1).await.unwrap();
        store.append(&event2).await.unwrap();
    }

    {
        let store = RedbEventStore::open(&path).unwrap();
        let events = store
            .query_by_subject("test.subject", None, 10)
            .await
            .unwrap();
        assert_eq!(events.len(), 2);

        let pending = store
            .query_by_status(EventStatus::Pending, 10)
            .await
            .unwrap();
        assert_eq!(pending.len(), 2);

        // Verify delivery records survive crash
        store
            .record_delivery(events[0].seq, "sub1", DeliveryStatus::Delivered, None, None)
            .await
            .unwrap();
        drop(store);
        let store2 = RedbEventStore::open(&path).unwrap();
        let events2 = store2
            .query_by_subject("test.subject", None, 10)
            .await
            .unwrap();
        assert_eq!(events2[0].deliveries.len(), 1);
    }
}

#[tokio::test]
async fn test_multi_subscriber() {
    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    let mut sub1 = bus.subscribe("test.subject", default_opts()).await.unwrap();
    let mut sub2 = bus.subscribe("test.subject", default_opts()).await.unwrap();

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
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    let _sub = bus.subscribe("test.subject", default_opts()).await.unwrap();

    let metrics = bus.metrics().await.unwrap();
    assert_eq!(metrics.subscription_count, 1);
}

#[tokio::test]
async fn test_unsubscribe() {
    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    let sub = bus.subscribe("test.subject", default_opts()).await.unwrap();
    bus.unsubscribe(&sub.id).await.unwrap();

    let subs = bus.list_subscriptions(None).await.unwrap();
    assert_eq!(subs.len(), 0);
}

#[tokio::test]
async fn test_invalid_subject_rejected() {
    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    assert!(bus.publish("", b"payload").await.is_err());
    assert!(bus.publish("test\0subject", b"payload").await.is_err());
}

// --- Phase 2 tests: Wildcards ---

#[tokio::test]
async fn test_wildcard_star_subscription() {
    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    let mut sub = bus
        .subscribe("channel.*.inbound", default_opts())
        .await
        .unwrap();

    bus.publish("channel.telegram.inbound", b"msg1")
        .await
        .unwrap();
    bus.publish("channel.email.inbound", b"msg2").await.unwrap();

    let msg1 = tokio::time::timeout(Duration::from_secs(1), sub.rx.recv())
        .await
        .unwrap();
    let msg2 = tokio::time::timeout(Duration::from_secs(1), sub.rx.recv())
        .await
        .unwrap();

    assert_eq!(msg1.map(|e| e.payload), Some(b"msg1".to_vec()));
    assert_eq!(msg2.map(|e| e.payload), Some(b"msg2".to_vec()));

    // Should NOT match 4-token subject
    bus.publish("channel.telegram.sub.inbound", b"nope")
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_millis(200), sub.rx.recv()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_wildcard_gt_subscription() {
    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    let mut sub = bus.subscribe("brain.>", default_opts()).await.unwrap();

    bus.publish("brain.content.store", b"a").await.unwrap();
    bus.publish("brain.entity.upsert.batch", b"b")
        .await
        .unwrap();

    let msg1 = tokio::time::timeout(Duration::from_secs(1), sub.rx.recv())
        .await
        .unwrap();
    let msg2 = tokio::time::timeout(Duration::from_secs(1), sub.rx.recv())
        .await
        .unwrap();

    assert_eq!(msg1.map(|e| e.payload), Some(b"a".to_vec()));
    assert_eq!(msg2.map(|e| e.payload), Some(b"b".to_vec()));

    // "brain" alone should NOT match "brain.>"
    bus.publish("brain", b"nope").await.unwrap();
    let result = tokio::time::timeout(Duration::from_millis(200), sub.rx.recv()).await;
    assert!(result.is_err());
}

// --- Phase 2 tests: Interceptors ---

struct UppercaseInterceptor;

#[async_trait::async_trait]
impl Interceptor for UppercaseInterceptor {
    async fn intercept(&self, _subject: &str, payload: &mut Vec<u8>) -> InterceptResult {
        payload.iter_mut().for_each(|b| *b = b.to_ascii_uppercase());
        InterceptResult::Modified
    }
}

struct DropInterceptor;

#[async_trait::async_trait]
impl Interceptor for DropInterceptor {
    async fn intercept(&self, _subject: &str, _payload: &mut Vec<u8>) -> InterceptResult {
        InterceptResult::Drop
    }
}

#[tokio::test]
async fn test_interceptor_modifies_payload() {
    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    bus.intercept("test", 5, Arc::new(UppercaseInterceptor), None)
        .await
        .unwrap();

    let mut sub = bus.subscribe("test", default_opts()).await.unwrap();

    bus.publish("test", b"hello world").await.unwrap();

    let msg = tokio::time::timeout(Duration::from_secs(1), sub.rx.recv())
        .await
        .unwrap();
    assert_eq!(msg.map(|e| e.payload), Some(b"HELLO WORLD".to_vec()));
}

#[tokio::test]
async fn test_interceptor_drops_event() {
    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    bus.intercept("test", 5, Arc::new(DropInterceptor), None)
        .await
        .unwrap();

    let mut sub = bus.subscribe("test", default_opts()).await.unwrap();

    bus.publish("test", b"hello").await.unwrap();

    let result = tokio::time::timeout(Duration::from_millis(200), sub.rx.recv()).await;
    assert!(
        result.is_err(),
        "event should have been dropped by interceptor"
    );
}

#[tokio::test]
async fn test_interceptor_chain_mutation_propagates() {
    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    // First interceptor uppercases (priority 5)
    bus.intercept("test", 5, Arc::new(UppercaseInterceptor), None)
        .await
        .unwrap();

    // Second interceptor also uppercases (priority 10) — idempotent, confirms chain
    bus.intercept("test", 10, Arc::new(UppercaseInterceptor), None)
        .await
        .unwrap();

    let mut sub = bus.subscribe("test", default_opts()).await.unwrap();

    bus.publish("test", b"hello").await.unwrap();

    let msg = tokio::time::timeout(Duration::from_secs(1), sub.rx.recv())
        .await
        .unwrap();
    assert_eq!(msg.map(|e| e.payload), Some(b"HELLO".to_vec()));
}

#[tokio::test]
async fn test_interceptor_with_wildcard() {
    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    // Interceptor on wildcard pattern
    bus.intercept("channel.*.inbound", 5, Arc::new(UppercaseInterceptor), None)
        .await
        .unwrap();

    let mut sub = bus
        .subscribe("channel.telegram.inbound", default_opts())
        .await
        .unwrap();

    bus.publish("channel.telegram.inbound", b"hello")
        .await
        .unwrap();

    let msg = tokio::time::timeout(Duration::from_secs(1), sub.rx.recv())
        .await
        .unwrap();
    assert_eq!(msg.map(|e| e.payload), Some(b"HELLO".to_vec()));
}

// --- Phase 2 tests: Event Filters ---

#[tokio::test]
async fn test_event_filter_field_equals() {
    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    let opts = SubscribeOpts {
        filter: Some(EventFilter::FieldEquals {
            path: "platform".to_string(),
            value: serde_json::json!("telegram"),
        }),
        ..default_opts()
    };
    let mut sub = bus.subscribe("channel.>", opts).await.unwrap();

    // Should match
    let tg = serde_json::to_vec(&serde_json::json!({"platform": "telegram"})).unwrap();
    bus.publish("channel.telegram.inbound", &tg).await.unwrap();

    let msg = tokio::time::timeout(Duration::from_secs(1), sub.rx.recv())
        .await
        .unwrap();
    assert!(msg.is_some());

    // Should NOT match
    let email = serde_json::to_vec(&serde_json::json!({"platform": "email"})).unwrap();
    bus.publish("channel.email.inbound", &email).await.unwrap();

    let result = tokio::time::timeout(Duration::from_millis(200), sub.rx.recv()).await;
    assert!(result.is_err());
}

// --- Issue #13 tests: Integration tests for fixed issues ---

#[tokio::test]
async fn test_wildcard_gt_not_allowed_in_middle() {
    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    // `>` in the middle of the pattern should be rejected
    let result = bus.subscribe("brain.>.content", default_opts()).await;
    assert!(
        result.is_err(),
        "pattern with > in middle should be rejected"
    );
}

#[tokio::test]
async fn test_empty_token_rejection() {
    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    // Double dots create empty tokens
    let result = bus.subscribe("test..subject", default_opts()).await;
    assert!(
        result.is_err(),
        "pattern with empty tokens should be rejected"
    );

    // Leading dot
    let result = bus.subscribe(".test.subject", default_opts()).await;
    assert!(
        result.is_err(),
        "pattern with leading dot should be rejected"
    );

    // Trailing dot
    let result = bus.subscribe("test.subject.", default_opts()).await;
    assert!(
        result.is_err(),
        "pattern with trailing dot should be rejected"
    );
}

struct TimeoutInterceptor;

#[async_trait::async_trait]
impl Interceptor for TimeoutInterceptor {
    async fn intercept(&self, _subject: &str, _payload: &mut Vec<u8>) -> InterceptResult {
        // Sleep longer than the timeout
        tokio::time::sleep(Duration::from_secs(2)).await;
        InterceptResult::Pass
    }
}

#[tokio::test]
async fn test_interceptor_timeout_behavior() {
    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    // Register an interceptor with a short timeout
    bus.intercept(
        "test",
        5,
        Arc::new(TimeoutInterceptor),
        Some(Duration::from_millis(100)),
    )
    .await
    .unwrap();

    // Register async subscriber
    let mut sub = bus.subscribe("test", default_opts()).await.unwrap();

    // Publish event
    bus.publish("test", b"hello").await.unwrap();

    // Subscriber should still receive the event (interceptor timed out and was skipped)
    let msg = tokio::time::timeout(Duration::from_secs(1), sub.rx.recv())
        .await
        .unwrap();
    assert_eq!(
        msg.map(|e| e.payload),
        Some(b"hello".to_vec()),
        "event should be delivered despite interceptor timeout"
    );
}

// --- Phase 3 tests: Request/Reply ---

#[tokio::test]
async fn test_request_reply_basic() {
    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    // Spawn a handler task
    let bus_clone = Arc::clone(&bus);
    tokio::spawn(async move {
        let mut sub = bus_clone
            .serve("test.request", 10, Duration::from_secs(5), None)
            .await
            .unwrap();

        while let Some((req_msg, replier)) = sub.rx.recv().await {
            let response = format!("echo: {}", String::from_utf8_lossy(&req_msg.payload));
            let _ = replier.reply(response.as_bytes()).await;
        }
    });

    // Give the handler time to register
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Send a request and wait for reply
    let reply = bus
        .request("test.request", b"hello", Duration::from_secs(1))
        .await
        .unwrap();

    assert_eq!(reply, b"echo: hello".to_vec());
}

#[tokio::test]
async fn test_request_reply_timeout() {
    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    // No handler registered for this subject
    let result = bus
        .request("test.request", b"hello", Duration::from_millis(100))
        .await;

    assert!(result.is_err());
}
