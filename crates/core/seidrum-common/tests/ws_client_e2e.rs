//! End-to-end integration tests for the WsClient adapter.
//!
//! These spin up a real seidrum-eventbus WS server on an ephemeral
//! port and exercise the WsClient against it.

use seidrum_common::ws_client::{WsClient, WsSubscription};
use seidrum_eventbus::test_utils::{pick_ephemeral_addr, wait_for_ws_ready};
use seidrum_eventbus::EventBusBuilder;
use std::sync::Arc;
use std::time::Duration;

/// Helper: start an eventbus with a WS server on an ephemeral port.
/// Returns the bus handles + the WS address.
async fn start_bus_with_ws() -> (seidrum_eventbus::BusHandles, std::net::SocketAddr) {
    let ws_addr = pick_ephemeral_addr();
    let store = Arc::new(seidrum_eventbus::storage::memory_store::InMemoryEventStore::new());
    let handles = EventBusBuilder::new()
        .storage(store)
        .with_websocket(ws_addr)
        .unsafe_allow_ws_dev_mode()
        .build_with_handles()
        .await
        .unwrap();
    wait_for_ws_ready(ws_addr).await;
    (handles, ws_addr)
}

#[tokio::test]
async fn test_ws_client_publish_and_subscribe() {
    let (handles, ws_addr) = start_bus_with_ws().await;
    let url = format!("ws://{}", ws_addr);

    let client = WsClient::connect(&url, "test-plugin").await.unwrap();
    assert!(client.is_connected());

    // Subscribe via WsClient.
    let mut sub: WsSubscription = client.subscribe("test.ws.>").await.unwrap();
    assert!(!sub.id.is_empty());

    // Publish via WsClient.
    client
        .publish_bytes("test.ws.hello", b"world")
        .await
        .unwrap();

    // Receive via subscription.
    let msg = tokio::time::timeout(Duration::from_secs(2), sub.next())
        .await
        .expect("should receive within 2s")
        .expect("subscription should not be closed");

    assert_eq!(msg.subject, "test.ws.hello");
    assert_eq!(msg.payload.as_ref(), b"world");
    assert!(msg.reply.is_none());

    // Unsubscribe.
    client.unsubscribe(&sub.id).await.unwrap();

    handles.shutdown_and_join().await;
}

#[tokio::test]
async fn test_ws_client_request_reply() {
    let (handles, ws_addr) = start_bus_with_ws().await;
    let url = format!("ws://{}", ws_addr);

    // Set up a request handler on the bus directly (in-process).
    let req_sub = handles
        .bus
        .serve("test.echo", 10, Duration::from_secs(5), None)
        .await
        .unwrap();

    // Spawn an echo handler.
    tokio::spawn(async move {
        let mut req_sub = req_sub;
        while let Some((req, replier)) = req_sub.rx.recv().await {
            let _ = replier.reply(&req.payload).await;
        }
    });

    // Connect a WsClient and make a request.
    let client = WsClient::connect(&url, "test-requester").await.unwrap();
    let response = client.request_bytes("test.echo", b"ping").await.unwrap();
    assert_eq!(response, b"ping");

    handles.shutdown_and_join().await;
}

#[tokio::test]
async fn test_ws_client_publish_envelope() {
    let (handles, ws_addr) = start_bus_with_ws().await;
    let url = format!("ws://{}", ws_addr);

    let client = WsClient::connect(&url, "envelope-test").await.unwrap();
    let mut sub = client.subscribe("envelope.subject").await.unwrap();

    let envelope = client
        .publish_envelope(
            "envelope.subject",
            Some("corr-123".to_string()),
            Some("scope-abc".to_string()),
            &serde_json::json!({"key": "value"}),
        )
        .await
        .unwrap();
    assert_eq!(envelope.event_type, "envelope.subject");
    assert_eq!(envelope.source, "envelope-test");
    assert_eq!(envelope.correlation_id, Some("corr-123".to_string()));

    // The subscriber receives the serialized EventEnvelope.
    let msg = tokio::time::timeout(Duration::from_secs(2), sub.next())
        .await
        .expect("should receive within 2s")
        .expect("subscription should not be closed");

    let received: seidrum_common::events::EventEnvelope =
        serde_json::from_slice(&msg.payload).unwrap();
    assert_eq!(received.event_type, "envelope.subject");
    assert_eq!(received.source, "envelope-test");

    handles.shutdown_and_join().await;
}

#[tokio::test]
async fn test_ws_client_typed_request() {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    struct Ping {
        message: String,
    }
    #[derive(Serialize, Deserialize, Debug)]
    struct Pong {
        echo: String,
    }

    let (handles, ws_addr) = start_bus_with_ws().await;
    let url = format!("ws://{}", ws_addr);

    // Echo handler that wraps the payload.
    let req_sub = handles
        .bus
        .serve("test.typed", 10, Duration::from_secs(5), None)
        .await
        .unwrap();
    tokio::spawn(async move {
        let mut req_sub = req_sub;
        while let Some((req, replier)) = req_sub.rx.recv().await {
            let ping: Ping = serde_json::from_slice(&req.payload).unwrap();
            let pong = Pong { echo: ping.message };
            let _ = replier.reply(&serde_json::to_vec(&pong).unwrap()).await;
        }
    });

    let client = WsClient::connect(&url, "typed-client").await.unwrap();
    let pong: Pong = client
        .request(
            "test.typed",
            &Ping {
                message: "hello".to_string(),
            },
        )
        .await
        .unwrap();
    assert_eq!(pong.echo, "hello");

    handles.shutdown_and_join().await;
}

#[tokio::test]
async fn test_ws_client_multiple_subscriptions() {
    let (handles, ws_addr) = start_bus_with_ws().await;
    let url = format!("ws://{}", ws_addr);
    let client = WsClient::connect(&url, "multi-sub").await.unwrap();

    let mut sub_a = client.subscribe("multi.a").await.unwrap();
    let mut sub_b = client.subscribe("multi.b").await.unwrap();

    client.publish_bytes("multi.a", b"alpha").await.unwrap();
    client.publish_bytes("multi.b", b"beta").await.unwrap();

    let msg_a = tokio::time::timeout(Duration::from_secs(2), sub_a.next())
        .await
        .unwrap()
        .unwrap();
    let msg_b = tokio::time::timeout(Duration::from_secs(2), sub_b.next())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(msg_a.payload.as_ref(), b"alpha");
    assert_eq!(msg_b.payload.as_ref(), b"beta");

    // Verify no cross-talk: a second publish to multi.a should not
    // appear on sub_b.
    client.publish_bytes("multi.a", b"alpha2").await.unwrap();
    let msg_a2 = tokio::time::timeout(Duration::from_secs(1), sub_a.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg_a2.payload.as_ref(), b"alpha2");

    let cross = tokio::time::timeout(Duration::from_millis(200), sub_b.next()).await;
    assert!(cross.is_err(), "sub_b should not receive multi.a events");

    handles.shutdown_and_join().await;
}

/// After calling `unsubscribe(id)`, subsequent publishes to the
/// matching subject must NOT be delivered to the subscription.
#[tokio::test]
async fn test_ws_client_unsubscribe_stops_delivery() {
    let (handles, ws_addr) = start_bus_with_ws().await;
    let url = format!("ws://{}", ws_addr);
    let client = WsClient::connect(&url, "unsub-test").await.unwrap();

    let mut sub = client.subscribe("unsub.subject").await.unwrap();
    let sub_id = sub.id.clone();

    // Verify delivery works before unsubscribe.
    client
        .publish_bytes("unsub.subject", b"before")
        .await
        .unwrap();
    let msg = tokio::time::timeout(Duration::from_secs(2), sub.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.payload.as_ref(), b"before");

    // Unsubscribe.
    client.unsubscribe(&sub_id).await.unwrap();

    // Give the server a beat to process the unsubscribe.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Publish again — the subscription should NOT receive it.
    client
        .publish_bytes("unsub.subject", b"after")
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_millis(300), sub.next()).await;
    assert!(
        result.is_err() || result.unwrap().is_none(),
        "subscription should not receive events after unsubscribe"
    );

    handles.shutdown_and_join().await;
}

/// Connecting to a non-existent server fails with a clean error.
#[tokio::test]
async fn test_ws_client_connect_refused() {
    // Port 1 is almost certainly not running a WS server.
    let result = WsClient::connect("ws://127.0.0.1:1", "test").await;
    assert!(result.is_err(), "connect to closed port should fail");
    let err = format!("{}", result.err().unwrap());
    assert!(
        err.contains("failed to connect"),
        "error should mention connection failure, got: {}",
        err
    );
}

/// After the server shuts down, operations must either succeed (TCP
/// buffer still draining) or fail with a clean error — they must
/// never hang. The WS server does not close existing connections on
/// graceful shutdown (spawned `serve_connection` tasks are detached),
/// so `is_connected()` may stay true for a while. This test verifies
/// the "no hang" property by wrapping operations in timeouts.
#[tokio::test]
async fn test_ws_client_operations_after_server_shutdown_do_not_hang() {
    let (handles, ws_addr) = start_bus_with_ws().await;
    let url = format!("ws://{}", ws_addr);
    let client = WsClient::connect(&url, "disconnect-test").await.unwrap();
    assert!(client.is_connected());

    // Verify it works before shutdown.
    client
        .publish_bytes("alive.subject", b"hello")
        .await
        .unwrap();

    // Shut down the server's accept loop (existing connections linger).
    handles.shutdown_and_join().await;

    // Try a subscribe (which requires a server response). With the
    // server's dispatch engine gone, the subscribe should either fail
    // or succeed from buffered state — but must not hang.
    let result =
        tokio::time::timeout(Duration::from_secs(3), client.subscribe("after.shutdown")).await;
    // We don't assert success or failure — either is fine. We only
    // assert it completed within the timeout (no hang).
    assert!(
        result.is_ok(),
        "subscribe after server shutdown should complete within 3s (not hang)"
    );
}

/// BusClient::connect retries when the server isn't up yet, then
/// succeeds once the server starts.
#[tokio::test]
async fn test_bus_client_connect_retries_until_server_ready() {
    use seidrum_common::bus_client::BusClient;
    use seidrum_eventbus::test_utils::pick_ephemeral_addr;

    let ws_addr = pick_ephemeral_addr();
    let url = format!("ws://{}", ws_addr);

    // Start the connect in a background task BEFORE the server is up.
    let url_clone = url.clone();
    let connect_task =
        tokio::spawn(async move { BusClient::connect(&url_clone, "retry-test").await });

    // Wait 500ms, THEN start the server. The connect should be
    // retrying during this window.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let store = Arc::new(seidrum_eventbus::storage::memory_store::InMemoryEventStore::new());
    let handles = seidrum_eventbus::EventBusBuilder::new()
        .storage(store)
        .with_websocket(ws_addr)
        .unsafe_allow_ws_dev_mode()
        .build_with_handles()
        .await
        .unwrap();
    seidrum_eventbus::test_utils::wait_for_ws_ready(ws_addr).await;

    // The connect should succeed now that the server is up.
    let result = tokio::time::timeout(Duration::from_secs(10), connect_task)
        .await
        .expect("connect task should finish")
        .expect("connect task should not panic");

    assert!(result.is_ok(), "BusClient should connect after retries");
    let client = result.unwrap();
    assert!(client.is_connected());

    handles.shutdown_and_join().await;
}

/// Configurable request timeout via `with_request_timeout`.
#[tokio::test]
async fn test_ws_client_request_timeout_configurable() {
    let (handles, ws_addr) = start_bus_with_ws().await;
    let url = format!("ws://{}", ws_addr);
    let client = WsClient::connect(&url, "timeout-test")
        .await
        .unwrap()
        .with_request_timeout(200); // 200ms — very short

    // No handler registered for this subject, so the request will time
    // out. With the default 5000ms this test would be slow; with 200ms
    // it should fail quickly.
    let start = tokio::time::Instant::now();
    let result = client.request_bytes("no.handler", b"ping").await;
    let elapsed = start.elapsed();

    assert!(result.is_err(), "request with no handler should time out");
    assert!(
        elapsed < Duration::from_secs(2),
        "should time out in ~200ms, not wait 5s (elapsed: {:?})",
        elapsed
    );

    handles.shutdown_and_join().await;
}
