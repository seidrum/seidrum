use seidrum_eventbus::storage::memory_store::InMemoryEventStore;
use seidrum_eventbus::{
    ChannelConfig, EventBusBuilder, EventStore, SubscribeOpts, SubscriptionMode,
};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn test_publish_deliver_end_to_end() {
    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    let opts = SubscribeOpts {
        priority: 10,
        mode: SubscriptionMode::Async,
        channel: ChannelConfig::InProcess,
        timeout: Duration::from_secs(5),
    };
    let mut sub = bus.subscribe("test.subject", opts).await.unwrap();

    let seq = bus.publish("test.subject", b"hello").await.unwrap();
    assert!(seq > 0);

    let msg = tokio::time::timeout(Duration::from_secs(1), sub.rx.recv())
        .await
        .unwrap();
    assert_eq!(msg, Some(b"hello".to_vec()));
}

#[tokio::test]
async fn test_crash_recovery_full() {
    use seidrum_eventbus::storage::redb_store::RedbEventStore;
    use seidrum_eventbus::storage::{EventStatus, StoredEvent};
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let path = temp.path().join("events.db");

    // Write some events directly to the store (simulating crash before delivery)
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
        // Store is dropped here, simulating a crash
    }

    // Reopen and verify events are still there (crash recovery)
    {
        let store = RedbEventStore::open(&path).unwrap();
        let events = store
            .query_by_subject("test.subject", None, 10)
            .await
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].payload, b"event1");
        assert_eq!(events[1].payload, b"event2");

        // Events should still be in Pending status (not delivered, since we "crashed")
        let pending = store
            .query_by_status(EventStatus::Pending, 10)
            .await
            .unwrap();
        assert_eq!(pending.len(), 2);
    }
}

#[tokio::test]
async fn test_multi_subscriber() {
    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    let opts = SubscribeOpts {
        priority: 10,
        mode: SubscriptionMode::Async,
        channel: ChannelConfig::InProcess,
        timeout: Duration::from_secs(5),
    };

    let mut sub1 = bus.subscribe("test.subject", opts.clone()).await.unwrap();
    let mut sub2 = bus.subscribe("test.subject", opts).await.unwrap();

    bus.publish("test.subject", b"message").await.unwrap();

    let msg1 = tokio::time::timeout(Duration::from_secs(1), sub1.rx.recv())
        .await
        .unwrap();
    let msg2 = tokio::time::timeout(Duration::from_secs(1), sub2.rx.recv())
        .await
        .unwrap();

    assert_eq!(msg1, Some(b"message".to_vec()));
    assert_eq!(msg2, Some(b"message".to_vec()));
}

#[tokio::test]
async fn test_metrics() {
    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    let opts = SubscribeOpts {
        priority: 10,
        mode: SubscriptionMode::Async,
        channel: ChannelConfig::InProcess,
        timeout: Duration::from_secs(5),
    };

    let _sub = bus.subscribe("test.subject", opts).await.unwrap();

    let metrics = bus.metrics().await.unwrap();
    assert_eq!(metrics.subscription_count, 1);
}

#[tokio::test]
async fn test_unsubscribe() {
    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await.unwrap();

    let opts = SubscribeOpts {
        priority: 10,
        mode: SubscriptionMode::Async,
        channel: ChannelConfig::InProcess,
        timeout: Duration::from_secs(5),
    };

    let sub = bus.subscribe("test.subject", opts).await.unwrap();
    bus.unsubscribe(&sub.id).await.unwrap();

    let subs = bus.list_subscriptions(None).await.unwrap();
    assert_eq!(subs.len(), 0);
}
