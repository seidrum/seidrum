//! Integration tests for the in-process BusClient backend.
//!
//! These exercise `BusClient::from_bus()` against a real eventbus
//! without any network transport — the same path the kernel uses.

use seidrum_common::bus_client::BusClient;
use seidrum_eventbus::storage::memory_store::InMemoryEventStore;
use seidrum_eventbus::EventBusBuilder;
use std::sync::Arc;
use std::time::Duration;

async fn build_bus() -> (seidrum_eventbus::BusHandles, BusClient) {
    let store = Arc::new(InMemoryEventStore::new());
    let handles = EventBusBuilder::new()
        .storage(store)
        .build_with_handles()
        .await
        .unwrap();
    let client = BusClient::from_bus(handles.bus.clone(), "test");
    (handles, client)
}

#[tokio::test]
async fn test_inprocess_publish_subscribe() {
    let (_handles, client) = build_bus().await;

    let mut sub = client.subscribe("test.inprocess.>").await.unwrap();
    client
        .publish_bytes("test.inprocess.hello", b"world".to_vec())
        .await
        .unwrap();

    let msg = tokio::time::timeout(Duration::from_secs(2), sub.next())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(msg.subject, "test.inprocess.hello");
    assert_eq!(msg.payload.as_ref(), b"world".to_vec());
}

#[tokio::test]
async fn test_inprocess_request_reply() {
    let (handles, client) = build_bus().await;

    // Set up an echo handler on the bus directly.
    let req_sub = handles
        .bus
        .serve("test.echo", 10, Duration::from_secs(5), None)
        .await
        .unwrap();
    tokio::spawn(async move {
        let mut req_sub = req_sub;
        while let Some((req, replier)) = req_sub.rx.recv().await {
            let _ = replier.reply(&req.payload).await;
        }
    });

    let response = client
        .request_bytes("test.echo", b"ping".to_vec())
        .await
        .unwrap();
    assert_eq!(response, b"ping".to_vec());
}

#[tokio::test]
async fn test_inprocess_typed_request() {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    struct Ping {
        msg: String,
    }
    #[derive(Serialize, Deserialize)]
    struct Pong {
        echo: String,
    }

    let (handles, client) = build_bus().await;

    let req_sub = handles
        .bus
        .serve("test.typed", 10, Duration::from_secs(5), None)
        .await
        .unwrap();
    tokio::spawn(async move {
        let mut req_sub = req_sub;
        while let Some((req, replier)) = req_sub.rx.recv().await {
            let ping: Ping = serde_json::from_slice(&req.payload).unwrap();
            let pong = Pong { echo: ping.msg };
            let _ = replier.reply(&serde_json::to_vec(&pong).unwrap()).await;
        }
    });

    let pong: Pong = client
        .request(
            "test.typed",
            &Ping {
                msg: "hello".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(pong.echo, "hello");
}

#[tokio::test]
async fn test_inprocess_publish_envelope() {
    let (_handles, client) = build_bus().await;

    let mut sub = client.subscribe("envelope.test").await.unwrap();
    let envelope = client
        .publish_envelope(
            "envelope.test",
            Some("corr-1".into()),
            Some("scope-a".into()),
            &serde_json::json!({"key": "val"}),
        )
        .await
        .unwrap();

    assert_eq!(envelope.event_type, "envelope.test");
    assert_eq!(envelope.source, "test");

    let msg = tokio::time::timeout(Duration::from_secs(2), sub.next())
        .await
        .unwrap()
        .unwrap();
    let received: seidrum_common::events::EventEnvelope =
        serde_json::from_slice(&msg.payload).unwrap();
    assert_eq!(received.event_type, "envelope.test");
}

#[tokio::test]
async fn test_inprocess_is_connected() {
    let (_handles, client) = build_bus().await;
    assert!(client.is_connected());
}
