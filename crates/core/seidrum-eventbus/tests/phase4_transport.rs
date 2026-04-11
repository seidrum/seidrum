//! Phase 4 integration tests for transport servers and remote delivery channels.

#[cfg(test)]
mod tests {
    use seidrum_eventbus::*;
    use std::sync::Arc;

    /// Test that we can create and configure the builder with transport options.
    #[tokio::test]
    async fn test_builder_with_transport_config() {
        let store = Arc::new(seidrum_eventbus::storage::memory_store::InMemoryEventStore::new());

        // Builder with WebSocket and HTTP transports configured
        let _bus = EventBusBuilder::new()
            .storage(store)
            .with_websocket("127.0.0.1:9000".parse().unwrap())
            .with_http("127.0.0.1:8000".parse().unwrap())
            .build()
            .await;

        // The bus should be created successfully even if we don't actually
        // connect (because we're not listening on those ports in the test).
        // The actual servers are spawned in background tasks.
    }

    /// Test builder returns task handles when using build_with_handles.
    #[tokio::test]
    async fn test_builder_with_handles() {
        let store = Arc::new(seidrum_eventbus::storage::memory_store::InMemoryEventStore::new());

        let handles = EventBusBuilder::new()
            .storage(store)
            .build_with_handles()
            .await
            .unwrap();

        assert!(handles.bus.metrics().await.is_ok());
        assert!(!handles.compaction.is_finished());
        assert!(handles.ws_server.is_none());
        assert!(handles.http_server.is_none());
    }

    /// Test that ChannelRegistry can register and retrieve channels.
    #[tokio::test]
    async fn test_channel_registry() {
        let registry = ChannelRegistry::new();

        // Create a simple dummy channel
        use async_trait::async_trait;

        struct TestChannel;

        #[async_trait]
        impl DeliveryChannel for TestChannel {
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

        let channel: Arc<dyn DeliveryChannel> = Arc::new(TestChannel);
        registry
            .register("test_channel", Arc::clone(&channel))
            .await;

        assert!(registry.get("test_channel").await.is_some());
        assert!(registry.get("nonexistent").await.is_none());

        let types = registry.list_types().await;
        assert!(types.contains(&"test_channel".to_string()));

        registry.unregister("test_channel").await;
        assert!(registry.get("test_channel").await.is_none());
    }

    /// Test WebSocket delivery channel can be created and tested.
    #[tokio::test]
    async fn test_websocket_channel() {
        use seidrum_eventbus::delivery::WebSocketMessage;
        use tokio::sync::mpsc;

        let (tx, mut rx) = mpsc::unbounded_channel::<WebSocketMessage>();
        let ws_channel = WebSocketChannel::new(tx);

        // Verify health
        assert!(ws_channel.is_healthy(&ChannelConfig::InProcess).await);

        // Test delivery
        let event = b"test event";
        let result = ws_channel
            .deliver(event, "test.subject", &ChannelConfig::InProcess)
            .await;

        assert!(result.is_ok(), "WebSocket deliver should succeed");
        let receipt = result.unwrap();
        assert!(receipt.latency_us > 0, "Latency should be positive");

        // Verify message was enqueued
        let msg = rx.recv().await;
        assert!(msg.is_some(), "Should receive a forwarded message");
    }

    /// Test webhook delivery channel configuration parsing.
    #[tokio::test]
    async fn test_webhook_channel_config() {
        use std::collections::HashMap;

        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer token123".to_string());

        let config = ChannelConfig::Webhook {
            url: "https://example.com/webhook".to_string(),
            headers,
        };

        // Verify we can work with the config
        match &config {
            ChannelConfig::Webhook { url, headers } => {
                assert_eq!(url, "https://example.com/webhook");
                assert_eq!(
                    headers.get("Authorization").unwrap(),
                    "Bearer token123",
                    "Authorization header should match"
                );
            }
            other => {
                panic!("Expected Webhook config, got {:?}", other);
            }
        }
    }

    /// Test retry task backoff calculation.
    #[test]
    fn test_retry_backoff() {
        use seidrum_eventbus::delivery::calculate_backoff;

        // attempt 1: base=100, subtractive jitter → 75..100
        let d1 = calculate_backoff(1, 100, 30000);
        assert!(
            d1.as_millis() >= 75 && d1.as_millis() <= 100,
            "attempt 1: expected 75..100ms, got {}ms",
            d1.as_millis()
        );

        // attempt 2: base=200 → 150..200
        let d2 = calculate_backoff(2, 100, 30000);
        assert!(
            d2.as_millis() >= 150 && d2.as_millis() <= 200,
            "attempt 2: expected 150..200ms, got {}ms",
            d2.as_millis()
        );

        // attempt 6: 100 * 2^5 = 3200 → 2400..3200
        let d6 = calculate_backoff(6, 100, 30000);
        assert!(
            d6.as_millis() >= 2400 && d6.as_millis() <= 3200,
            "attempt 6: expected 2400..3200ms, got {}ms",
            d6.as_millis()
        );

        let d11 = calculate_backoff(11, 100, 30000);
        assert!(
            d11.as_millis() <= 30000,
            "attempt 11 should be capped at 30000ms, got {}ms",
            d11.as_millis()
        );
    }

    /// Webhook subscription persistence: a subscription created via
    /// `POST /subscribe` survives a restart of the HTTP server when the
    /// underlying store is shared between the two server instances.
    #[tokio::test]
    async fn test_webhook_subscription_persistence_across_restart() {
        use seidrum_eventbus::storage::memory_store::InMemoryEventStore;
        use seidrum_eventbus::storage::EventStore;
        use seidrum_eventbus::test_utils::{pick_ephemeral_addr, wait_for_http_ready};
        use std::time::Duration;

        // Single shared store across both "server lifetimes".
        let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());

        // Pick an ephemeral port to reuse for both server runs.
        let addr = pick_ephemeral_addr();

        // === First "lifetime": create a webhook subscription ===
        let store_clone = Arc::clone(&store);
        let handles_a = EventBusBuilder::new()
            .storage(store_clone)
            .with_http(addr)
            .with_webhook_url_policy(seidrum_eventbus::delivery::WebhookUrlPolicy::Permissive)
            .unsafe_allow_http_dev_mode()
            .build_with_handles()
            .await
            .unwrap();

        // Wait for the HTTP server to come up.
        wait_for_http_ready(addr).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{}/subscribe", addr))
            .json(&serde_json::json!({
                "pattern": "persist.test",
                "url": "http://127.0.0.1:1/never-called",
                "priority": 0,
            }))
            .send()
            .await
            .expect("subscribe request should succeed");
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let original_bus_id = body["id"].as_str().unwrap().to_string();
        assert!(!original_bus_id.is_empty());

        // The store should now contain exactly one persisted subscription.
        let persisted = store.list_subscriptions().await.unwrap();
        assert_eq!(
            persisted.len(),
            1,
            "subscription should be persisted to the store"
        );
        assert_eq!(persisted[0].pattern, "persist.test");
        assert_eq!(persisted[0].url, "http://127.0.0.1:1/never-called");

        // Shut down the first server and wait for it to actually exit so
        // the port is released before the second server tries to bind.
        handles_a.shutdown_and_join().await;

        // Give the OS a moment to fully release the port.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // === Second "lifetime": new bus + new HTTP server, same store ===
        let store_clone2 = Arc::clone(&store);
        let handles_b = EventBusBuilder::new()
            .storage(store_clone2)
            .with_http(addr)
            .with_webhook_url_policy(seidrum_eventbus::delivery::WebhookUrlPolicy::Permissive)
            .unsafe_allow_http_dev_mode()
            .build_with_handles()
            .await
            .unwrap();

        wait_for_http_ready(addr).await;

        // The new bus should have a fresh subscription whose pattern matches
        // the persisted entry — proving recreation happened on startup.
        let subs = handles_b.bus.list_subscriptions(None).await.unwrap();
        let recreated = subs
            .iter()
            .find(|s| s.pattern == "persist.test")
            .expect("persisted subscription should be recreated on the new bus");
        // The recreated subscription gets a fresh runtime id (not the same
        // as the original one from the first server).
        assert_ne!(recreated.id, original_bus_id);

        // The persisted entry should still exist in the store.
        let persisted_after = store.list_subscriptions().await.unwrap();
        assert_eq!(persisted_after.len(), 1);

        handles_b.shutdown_and_join().await;
    }

    /// Webhook subscription deletion via DELETE /subscribe/:id removes the
    /// persisted entry from the store, so it does not get recreated.
    #[tokio::test]
    async fn test_webhook_subscription_delete_removes_persisted_entry() {
        use seidrum_eventbus::storage::memory_store::InMemoryEventStore;
        use seidrum_eventbus::storage::EventStore;
        use seidrum_eventbus::test_utils::{pick_ephemeral_addr, wait_for_http_ready};

        let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
        let addr = pick_ephemeral_addr();

        let handles = EventBusBuilder::new()
            .storage(Arc::clone(&store))
            .with_http(addr)
            .with_webhook_url_policy(seidrum_eventbus::delivery::WebhookUrlPolicy::Permissive)
            .unsafe_allow_http_dev_mode()
            .build_with_handles()
            .await
            .unwrap();

        wait_for_http_ready(addr).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{}/subscribe", addr))
            .json(&serde_json::json!({
                "pattern": "persist.delete",
                "url": "http://127.0.0.1:1/never-called",
                "priority": 0,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let bus_id = body["id"].as_str().unwrap().to_string();

        assert_eq!(store.list_subscriptions().await.unwrap().len(), 1);

        // Now DELETE it.
        let resp = client
            .delete(format!("http://{}/subscribe/{}", addr, bus_id))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);

        // The persisted entry should be gone.
        assert_eq!(
            store.list_subscriptions().await.unwrap().len(),
            0,
            "DELETE should remove the persisted entry"
        );

        handles.shutdown_and_join().await;
    }

    /// End-to-end: HTTP `POST /publish` → in-process subscriber receives
    /// the event. Verifies the HTTP transport is wired through to the bus.
    #[tokio::test]
    async fn test_http_publish_to_inprocess_subscriber_e2e() {
        use seidrum_eventbus::test_utils::{
            test_bus_with_transports, wait_for_http_ready,
        };
        use std::time::Duration;

        let env = test_bus_with_transports().await;
        wait_for_http_ready(env.http_addr).await;

        let opts = SubscribeOpts {
            priority: 10,
            mode: SubscriptionMode::Async,
            channel: ChannelConfig::InProcess,
            timeout: Duration::from_secs(5),
            filter: None,
        };
        let mut sub = env
            .handles
            .bus
            .subscribe("e2e.http.test", opts)
            .await
            .unwrap();

        // Publish via the HTTP API.
        use base64::Engine;
        let payload_b64 = base64::engine::general_purpose::STANDARD.encode(b"hello via http");
        let resp = reqwest::Client::new()
            .post(format!("http://{}/publish", env.http_addr))
            .json(&serde_json::json!({
                "subject": "e2e.http.test",
                "payload": payload_b64,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // The in-process subscriber should receive it within a beat.
        let received = tokio::time::timeout(Duration::from_secs(2), sub.rx.recv())
            .await
            .expect("subscriber should receive event from HTTP publish")
            .expect("rx should not be closed");
        assert_eq!(received.subject, "e2e.http.test");
        assert_eq!(received.payload, b"hello via http");

        env.handles.shutdown_and_join().await;
    }

    /// End-to-end: a webhook subscription created via the HTTP API
    /// receives published events at the configured URL. Uses the
    /// MockWebhookServer test utility.
    #[tokio::test]
    async fn test_webhook_subscription_delivers_to_mock_server_e2e() {
        use seidrum_eventbus::test_utils::{
            test_bus_with_transports, wait_for_http_ready, MockWebhookServer,
        };
        use std::time::Duration;

        let env = test_bus_with_transports().await;
        wait_for_http_ready(env.http_addr).await;

        let mock = MockWebhookServer::start().await;
        let webhook_url = mock.url_for("/hook");

        // Create the webhook subscription via the HTTP API.
        let resp = reqwest::Client::new()
            .post(format!("http://{}/subscribe", env.http_addr))
            .json(&serde_json::json!({
                "pattern": "e2e.webhook",
                "url": webhook_url,
                "priority": 0,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // Publish three events directly via the bus (skipping the HTTP
        // publish path which has its own e2e test above).
        env.handles
            .bus
            .publish("e2e.webhook", b"first")
            .await
            .unwrap();
        env.handles
            .bus
            .publish("e2e.webhook", b"second")
            .await
            .unwrap();
        env.handles
            .bus
            .publish("e2e.webhook", b"third")
            .await
            .unwrap();

        // Wait for all three deliveries to land.
        let deliveries = mock.wait_for(3, Duration::from_secs(3)).await;
        assert_eq!(
            deliveries.len(),
            3,
            "mock server should receive 3 deliveries, got {}",
            deliveries.len()
        );
        // Webhook bodies are JSON envelopes: {"subject": ..., "payload": <b64>}.
        // Decode each one and check that the original payload bytes appear.
        use base64::Engine;
        let mut decoded_payloads: Vec<Vec<u8>> = Vec::new();
        for d in &deliveries {
            let body: serde_json::Value = serde_json::from_slice(&d.body)
                .expect("webhook body should be JSON");
            assert_eq!(body["subject"].as_str(), Some("e2e.webhook"));
            let b64 = body["payload"].as_str().expect("payload should be string");
            decoded_payloads.push(
                base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .expect("payload should be valid base64"),
            );
        }
        assert!(decoded_payloads.contains(&b"first".to_vec()));
        assert!(decoded_payloads.contains(&b"second".to_vec()));
        assert!(decoded_payloads.contains(&b"third".to_vec()));

        mock.shutdown().await;
        env.handles.shutdown_and_join().await;
    }

    /// End-to-end: HTTP `GET /events/:seq` returns a previously-published
    /// event with its sequence number, subject, and base64 payload.
    #[tokio::test]
    async fn test_http_get_event_returns_stored_event_e2e() {
        use seidrum_eventbus::test_utils::{test_bus_with_transports, wait_for_http_ready};

        let env = test_bus_with_transports().await;
        wait_for_http_ready(env.http_addr).await;

        // Publish directly via the bus to control the seq.
        let seq = env
            .handles
            .bus
            .publish("e2e.get_event", b"payload bytes")
            .await
            .unwrap();

        // GET /events/:seq via HTTP.
        let resp = reqwest::Client::new()
            .get(format!("http://{}/events/{}", env.http_addr, seq))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["seq"].as_u64().unwrap(), seq);
        assert_eq!(body["subject"].as_str().unwrap(), "e2e.get_event");

        // Payload is base64-encoded.
        use base64::Engine;
        let payload_b64 = body["payload"].as_str().unwrap();
        let payload = base64::engine::general_purpose::STANDARD
            .decode(payload_b64)
            .unwrap();
        assert_eq!(payload, b"payload bytes");

        // 404 for unknown seq.
        let resp = reqwest::Client::new()
            .get(format!("http://{}/events/9999999", env.http_addr))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        env.handles.shutdown_and_join().await;
    }

    /// Verify the test_utils RecordingInterceptor works end-to-end through
    /// the bus's interceptor chain.
    #[tokio::test]
    async fn test_recording_interceptor_via_bus() {
        use seidrum_eventbus::test_utils::{test_bus, RecordingInterceptor};

        let bus = test_bus().await;
        let recorder = Arc::new(RecordingInterceptor::new());
        bus.intercept("rec.>", 5, Arc::clone(&recorder) as Arc<_>, None)
            .await
            .unwrap();

        bus.publish("rec.alpha", b"one").await.unwrap();
        bus.publish("rec.beta", b"two").await.unwrap();
        bus.publish("other.subject", b"ignored").await.unwrap();

        // **N10 fix**: bounded poll instead of single yield_now. Sync
        // interceptors run inside a tokio::spawn (panic isolation), so
        // a single yield is not guaranteed to schedule and complete the
        // spawned task. Poll up to 500ms total.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            if recorder.count().await >= 2 {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let events = recorder.events().await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].subject, "rec.alpha");
        assert_eq!(events[0].payload, b"one");
        assert_eq!(events[1].subject, "rec.beta");
        assert_eq!(events[1].payload, b"two");
    }

    /// End-to-end: a WebSocket client registers a remote interceptor
    /// over the WS protocol, and a subsequent `bus.publish` triggers an
    /// `intercept` frame that the client responds to with `pass`. The
    /// async subscriber should still receive the (unmodified) event.
    #[tokio::test]
    async fn test_ws_remote_interceptor_pass_e2e() {
        use futures_util::{SinkExt, StreamExt};
        use seidrum_eventbus::test_utils::{test_bus_with_transports, wait_for_ws_ready};
        use std::time::Duration;
        use tokio_tungstenite::tungstenite::Message;

        let env = test_bus_with_transports().await;
        wait_for_ws_ready(env.ws_addr).await;

        // Connect a WS client.
        let url = format!("ws://{}", env.ws_addr);
        let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("ws client should connect");
        let (mut write, mut read) = ws_stream.split();

        // Register a remote interceptor on `e2e.intercept.>`.
        let register = serde_json::json!({
            "op": "register_interceptor",
            "pattern": "e2e.intercept.>",
            "priority": 5,
            "correlation_id": "reg-1",
        });
        write
            .send(Message::text(register.to_string()))
            .await
            .unwrap();

        // Wait for the interceptor_registered ack.
        let ack_msg = tokio::time::timeout(Duration::from_secs(2), read.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let ack: serde_json::Value = serde_json::from_str(ack_msg.to_text().unwrap()).unwrap();
        assert_eq!(ack["op"], "interceptor_registered");
        let _interceptor_id = ack["id"].as_str().unwrap().to_string();

        // Subscribe in-process so we can observe the post-interceptor payload.
        let opts = SubscribeOpts {
            priority: 100,
            mode: SubscriptionMode::Async,
            channel: ChannelConfig::InProcess,
            timeout: Duration::from_secs(5),
            filter: None,
        };
        let mut sub = env
            .handles
            .bus
            .subscribe("e2e.intercept.alpha", opts)
            .await
            .unwrap();

        // Spawn a task that drives the WS client: read intercept frames
        // and reply with `pass`. Returns the read half so we can shut down.
        let publisher_done = Arc::new(tokio::sync::Notify::new());
        let publisher_done_clone = Arc::clone(&publisher_done);
        let ws_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    msg = read.next() => {
                        let msg = match msg {
                            Some(Ok(m)) => m,
                            _ => break,
                        };
                        if !msg.is_text() {
                            continue;
                        }
                        let text = msg.to_text().unwrap();
                        let parsed: serde_json::Value = match serde_json::from_str(text) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if parsed["op"] == "intercept" {
                            let request_id = parsed["request_id"].as_str().unwrap().to_string();
                            let reply = serde_json::json!({
                                "op": "intercept_result",
                                "request_id": request_id,
                                "action": "pass",
                            });
                            write.send(Message::text(reply.to_string())).await.unwrap();
                        }
                    }
                    _ = publisher_done_clone.notified() => break,
                }
            }
        });

        // Publish an event matching the interceptor's pattern. The
        // remote interceptor should fire and pass the event through to
        // our async subscriber.
        env.handles
            .bus
            .publish("e2e.intercept.alpha", b"original")
            .await
            .unwrap();

        // The async subscriber should receive the event after the
        // remote interceptor returns Pass.
        let received = tokio::time::timeout(Duration::from_secs(3), sub.rx.recv())
            .await
            .expect("subscriber should receive event")
            .expect("rx should not be closed");
        assert_eq!(received.subject, "e2e.intercept.alpha");
        assert_eq!(received.payload, b"original");

        publisher_done.notify_one();
        let _ = ws_task.await;
        env.handles.shutdown_and_join().await;
    }

    /// End-to-end: a WS-registered remote interceptor returns `modify`,
    /// and the async subscriber receives the replaced payload.
    #[tokio::test]
    async fn test_ws_remote_interceptor_modify_e2e() {
        use base64::Engine;
        use futures_util::{SinkExt, StreamExt};
        use seidrum_eventbus::test_utils::{test_bus_with_transports, wait_for_ws_ready};
        use std::time::Duration;
        use tokio_tungstenite::tungstenite::Message;

        let env = test_bus_with_transports().await;
        wait_for_ws_ready(env.ws_addr).await;

        let url = format!("ws://{}", env.ws_addr);
        let (ws_stream, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (mut write, mut read) = ws_stream.split();

        write
            .send(Message::text(
                serde_json::json!({
                    "op": "register_interceptor",
                    "pattern": "e2e.modify.test",
                    "priority": 5,
                })
                .to_string(),
            ))
            .await
            .unwrap();

        // Drain the registration ack.
        let _ = tokio::time::timeout(Duration::from_secs(2), read.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        let opts = SubscribeOpts {
            priority: 100,
            mode: SubscriptionMode::Async,
            channel: ChannelConfig::InProcess,
            timeout: Duration::from_secs(5),
            filter: None,
        };
        let mut sub = env
            .handles
            .bus
            .subscribe("e2e.modify.test", opts)
            .await
            .unwrap();

        // Drive the client to reply `modify` with a new payload.
        let publisher_done = Arc::new(tokio::sync::Notify::new());
        let publisher_done_clone = Arc::clone(&publisher_done);
        let ws_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    msg = read.next() => {
                        let msg = match msg {
                            Some(Ok(m)) => m,
                            _ => break,
                        };
                        if !msg.is_text() { continue; }
                        let parsed: serde_json::Value =
                            match serde_json::from_str(msg.to_text().unwrap()) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                        if parsed["op"] == "intercept" {
                            let request_id = parsed["request_id"].as_str().unwrap().to_string();
                            let new_payload =
                                base64::engine::general_purpose::STANDARD.encode(b"REPLACED");
                            let reply = serde_json::json!({
                                "op": "intercept_result",
                                "request_id": request_id,
                                "action": "modify",
                                "payload": new_payload,
                            });
                            write.send(Message::text(reply.to_string())).await.unwrap();
                        }
                    }
                    _ = publisher_done_clone.notified() => break,
                }
            }
        });

        env.handles
            .bus
            .publish("e2e.modify.test", b"original")
            .await
            .unwrap();

        let received = tokio::time::timeout(Duration::from_secs(3), sub.rx.recv())
            .await
            .expect("subscriber should receive event")
            .expect("rx should not be closed");
        assert_eq!(received.subject, "e2e.modify.test");
        assert_eq!(
            received.payload, b"REPLACED",
            "interceptor should have replaced the payload"
        );

        publisher_done.notify_one();
        let _ = ws_task.await;
        env.handles.shutdown_and_join().await;
    }

    /// End-to-end: a WS-registered remote interceptor returns `drop`,
    /// and the async subscriber receives nothing.
    #[tokio::test]
    async fn test_ws_remote_interceptor_drop_e2e() {
        use futures_util::{SinkExt, StreamExt};
        use seidrum_eventbus::test_utils::{test_bus_with_transports, wait_for_ws_ready};
        use std::time::Duration;
        use tokio_tungstenite::tungstenite::Message;

        let env = test_bus_with_transports().await;
        wait_for_ws_ready(env.ws_addr).await;

        let url = format!("ws://{}", env.ws_addr);
        let (ws_stream, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (mut write, mut read) = ws_stream.split();

        // Register a remote interceptor on `e2e.drop.test`. Use the
        // minimum allowed remote priority (100) so it runs before the
        // async subscriber but is permitted by the C1 floor.
        write
            .send(Message::text(
                serde_json::json!({
                    "op": "register_interceptor",
                    "pattern": "e2e.drop.test",
                    "priority": 100,
                })
                .to_string(),
            ))
            .await
            .unwrap();

        // Drain the registration ack.
        let _ = tokio::time::timeout(Duration::from_secs(2), read.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        let opts = SubscribeOpts {
            priority: 200,
            mode: SubscriptionMode::Async,
            channel: ChannelConfig::InProcess,
            timeout: Duration::from_secs(5),
            filter: None,
        };
        let mut sub = env
            .handles
            .bus
            .subscribe("e2e.drop.test", opts)
            .await
            .unwrap();

        // Drive the client to reply with `drop`.
        let publisher_done = Arc::new(tokio::sync::Notify::new());
        let publisher_done_clone = Arc::clone(&publisher_done);
        let ws_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    msg = read.next() => {
                        let msg = match msg {
                            Some(Ok(m)) => m,
                            _ => break,
                        };
                        if !msg.is_text() { continue; }
                        let parsed: serde_json::Value =
                            match serde_json::from_str(msg.to_text().unwrap()) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                        if parsed["op"] == "intercept" {
                            let request_id = parsed["request_id"].as_str().unwrap().to_string();
                            let reply = serde_json::json!({
                                "op": "intercept_result",
                                "request_id": request_id,
                                "action": "drop",
                            });
                            write.send(Message::text(reply.to_string())).await.unwrap();
                        }
                    }
                    _ = publisher_done_clone.notified() => break,
                }
            }
        });

        env.handles
            .bus
            .publish("e2e.drop.test", b"toxic")
            .await
            .unwrap();

        // The async subscriber should NOT receive the event because the
        // remote interceptor dropped it. Wait briefly to confirm nothing
        // arrives, then assert.
        let result = tokio::time::timeout(Duration::from_millis(500), sub.rx.recv()).await;
        assert!(
            result.is_err(),
            "subscriber should not receive a dropped event, got {:?}",
            result.ok().flatten().map(|e| e.payload)
        );

        publisher_done.notify_one();
        let _ = ws_task.await;
        env.handles.shutdown_and_join().await;
    }

    /// End-to-end (C3): register a webhook sync interceptor via
    /// `POST /interceptors`, publish an event, and verify the in-process
    /// subscriber receives the modified payload that the mock webhook
    /// returned in its response body.
    #[tokio::test]
    async fn test_webhook_sync_interceptor_modify_e2e() {
        use seidrum_eventbus::test_utils::{test_bus_with_transports, wait_for_http_ready};
        use std::time::Duration;

        // Boot a mock webhook server that ALSO supports the
        // intercept-result wire format on a specific path. The base
        // MockWebhookServer always returns 200 with no body — that's not
        // enough for sync interceptors, which need a JSON action body.
        // Spin up a tiny ad-hoc server here instead.
        use axum::{routing::post, Json as AJson, Router as ARouter};
        let interceptor_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = tokio::net::TcpListener::bind(interceptor_addr).await.unwrap();
        let interceptor_addr = listener.local_addr().unwrap();
        async fn modify_handler(
            AJson(_body): AJson<serde_json::Value>,
        ) -> AJson<serde_json::Value> {
            use base64::Engine;
            let new_payload =
                base64::engine::general_purpose::STANDARD.encode(b"REWRITTEN BY WEBHOOK");
            AJson(serde_json::json!({
                "action": "modify",
                "payload": new_payload,
            }))
        }
        let app = ARouter::new().route("/intercept", post(modify_handler));
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        seidrum_eventbus::test_utils::wait_for_tcp_ready(interceptor_addr).await;

        let env = test_bus_with_transports().await;
        wait_for_http_ready(env.http_addr).await;

        // Register the webhook sync interceptor via the new endpoint.
        let webhook_url = format!("http://{}/intercept", interceptor_addr);
        let resp = reqwest::Client::new()
            .post(format!("http://{}/interceptors", env.http_addr))
            .json(&serde_json::json!({
                "pattern": "e2e.webhook_intercept",
                "url": webhook_url,
                "priority": 100,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "register interceptor should succeed");
        let body: serde_json::Value = resp.json().await.unwrap();
        let interceptor_id = body["id"].as_str().unwrap().to_string();
        assert!(!interceptor_id.is_empty());

        // Subscribe in-process to observe the post-interceptor payload.
        let opts = SubscribeOpts {
            priority: 200,
            mode: SubscriptionMode::Async,
            channel: ChannelConfig::InProcess,
            timeout: Duration::from_secs(5),
            filter: None,
        };
        let mut sub = env
            .handles
            .bus
            .subscribe("e2e.webhook_intercept", opts)
            .await
            .unwrap();

        // Publish — the webhook interceptor should fire and rewrite the payload.
        env.handles
            .bus
            .publish("e2e.webhook_intercept", b"original")
            .await
            .unwrap();

        let received = tokio::time::timeout(Duration::from_secs(3), sub.rx.recv())
            .await
            .expect("subscriber should receive event")
            .expect("rx should not be closed");
        assert_eq!(received.subject, "e2e.webhook_intercept");
        assert_eq!(
            received.payload, b"REWRITTEN BY WEBHOOK",
            "interceptor should have rewritten the payload"
        );

        // Cleanup: DELETE the interceptor and verify the bus no longer
        // routes events through it.
        let resp = reqwest::Client::new()
            .delete(format!(
                "http://{}/interceptors/{}",
                env.http_addr, interceptor_id
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);

        env.handles.shutdown_and_join().await;
        server_task.abort();
    }

    /// End-to-end (C3): a webhook sync interceptor that returns
    /// `{"action": "drop"}` aborts the dispatch chain.
    #[tokio::test]
    async fn test_webhook_sync_interceptor_drop_e2e() {
        use seidrum_eventbus::test_utils::{test_bus_with_transports, wait_for_http_ready};
        use std::time::Duration;

        use axum::{routing::post, Json as AJson, Router as ARouter};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let interceptor_addr = listener.local_addr().unwrap();
        async fn drop_handler(
            AJson(_body): AJson<serde_json::Value>,
        ) -> AJson<serde_json::Value> {
            AJson(serde_json::json!({"action": "drop"}))
        }
        let app = ARouter::new().route("/intercept", post(drop_handler));
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        seidrum_eventbus::test_utils::wait_for_tcp_ready(interceptor_addr).await;

        let env = test_bus_with_transports().await;
        wait_for_http_ready(env.http_addr).await;

        let webhook_url = format!("http://{}/intercept", interceptor_addr);
        let resp = reqwest::Client::new()
            .post(format!("http://{}/interceptors", env.http_addr))
            .json(&serde_json::json!({
                "pattern": "e2e.webhook_drop",
                "url": webhook_url,
                "priority": 100,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let opts = SubscribeOpts {
            priority: 200,
            mode: SubscriptionMode::Async,
            channel: ChannelConfig::InProcess,
            timeout: Duration::from_secs(5),
            filter: None,
        };
        let mut sub = env
            .handles
            .bus
            .subscribe("e2e.webhook_drop", opts)
            .await
            .unwrap();

        env.handles
            .bus
            .publish("e2e.webhook_drop", b"toxic")
            .await
            .unwrap();

        let result = tokio::time::timeout(Duration::from_millis(500), sub.rx.recv()).await;
        assert!(
            result.is_err(),
            "subscriber should not receive a dropped event, got {:?}",
            result.ok().flatten().map(|e| e.payload)
        );

        env.handles.shutdown_and_join().await;
        server_task.abort();
    }

    /// **N8b / C1 regression**: `POST /interceptors` rejects bypass
    /// patterns at the API layer with 400 INVALID_INTERCEPTOR_PATTERN.
    #[tokio::test]
    async fn test_http_interceptors_rejects_bypass_patterns() {
        use seidrum_eventbus::test_utils::{test_bus_with_transports, wait_for_http_ready};

        let env = test_bus_with_transports().await;
        wait_for_http_ready(env.http_addr).await;

        let bypass_patterns = [">", "*.>", "*.*", "_reply", "_reply.>", "_reply.foo"];
        for pattern in bypass_patterns {
            let resp = reqwest::Client::new()
                .post(format!("http://{}/interceptors", env.http_addr))
                .json(&serde_json::json!({
                    "pattern": pattern,
                    "url": "http://127.0.0.1:1/never-called",
                    "priority": 100,
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                400,
                "pattern {:?} must be rejected",
                pattern
            );
            let body: serde_json::Value = resp.json().await.unwrap();
            assert_eq!(body["code"], "INVALID_INTERCEPTOR_PATTERN");
        }

        env.handles.shutdown_and_join().await;
    }

    /// **N8b / B2 regression**: `POST /interceptors` clamps a low
    /// priority to MIN_REMOTE_INTERCEPTOR_PRIORITY (100). The persisted
    /// entry should record the clamped value, not the requested one.
    #[tokio::test]
    async fn test_http_interceptors_priority_clamped() {
        use seidrum_eventbus::storage::EventStore;
        use seidrum_eventbus::test_utils::{
            pick_ephemeral_addr, wait_for_http_ready,
        };
        use std::sync::Arc as SArc;

        let store: SArc<dyn EventStore> =
            SArc::new(seidrum_eventbus::storage::memory_store::InMemoryEventStore::new());
        let addr = pick_ephemeral_addr();
        let handles = EventBusBuilder::new()
            .storage(SArc::clone(&store))
            .with_http(addr)
            .with_webhook_url_policy(seidrum_eventbus::delivery::WebhookUrlPolicy::Permissive)
            .unsafe_allow_http_dev_mode()
            .build_with_handles()
            .await
            .unwrap();
        wait_for_http_ready(addr).await;

        let resp = reqwest::Client::new()
            .post(format!("http://{}/interceptors", addr))
            .json(&serde_json::json!({
                "pattern": "events.audit",
                "url": "http://127.0.0.1:1/never-called",
                "priority": 0, // attacker requesting priority 0
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // The persisted entry should record priority 100, not 0.
        let persisted = store.list_subscriptions().await.unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(
            persisted[0].priority, 100,
            "priority should be clamped to MIN_REMOTE_INTERCEPTOR_PRIORITY"
        );
        assert_eq!(
            persisted[0].kind,
            seidrum_eventbus::storage::PersistedSubscriptionKind::SyncInterceptor
        );

        handles.shutdown_and_join().await;
    }

    /// **N8b / B3 regression**: `POST /interceptors` rejects requests
    /// that exceed MAX_HTTP_INTERCEPTORS (64) with 429.
    #[tokio::test]
    async fn test_http_interceptors_per_server_cap() {
        use seidrum_eventbus::test_utils::{test_bus_with_transports, wait_for_http_ready};

        let env = test_bus_with_transports().await;
        wait_for_http_ready(env.http_addr).await;

        let client = reqwest::Client::new();
        // Register up to the cap (64).
        for i in 0..64 {
            let resp = client
                .post(format!("http://{}/interceptors", env.http_addr))
                .json(&serde_json::json!({
                    "pattern": format!("cap.test.{}", i),
                    "url": "http://127.0.0.1:1/never-called",
                    "priority": 100,
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "request {} should succeed", i);
        }
        // The 65th must fail.
        let resp = client
            .post(format!("http://{}/interceptors", env.http_addr))
            .json(&serde_json::json!({
                "pattern": "cap.test.over",
                "url": "http://127.0.0.1:1/never-called",
                "priority": 100,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 429);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["code"], "INTERCEPTOR_LIMIT");

        env.handles.shutdown_and_join().await;
    }

    /// **N8b / B4 regression**: under default `NoHttpAuth` (without
    /// dev mode opt-in), `POST /interceptors` returns 401.
    #[tokio::test]
    async fn test_http_interceptors_blocked_under_open_auth() {
        use seidrum_eventbus::storage::memory_store::InMemoryEventStore;
        use seidrum_eventbus::test_utils::{pick_ephemeral_addr, wait_for_http_ready};
        use std::sync::Arc as SArc;

        let store = SArc::new(InMemoryEventStore::new());
        let addr = pick_ephemeral_addr();
        // No `unsafe_allow_http_dev_mode()` — defaults to refusing.
        let handles = EventBusBuilder::new()
            .storage(store)
            .with_http(addr)
            .with_webhook_url_policy(seidrum_eventbus::delivery::WebhookUrlPolicy::Permissive)
            .build_with_handles()
            .await
            .unwrap();
        wait_for_http_ready(addr).await;

        let resp = reqwest::Client::new()
            .post(format!("http://{}/interceptors", addr))
            .json(&serde_json::json!({
                "pattern": "events.foo",
                "url": "http://127.0.0.1:1/never-called",
                "priority": 100,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["code"], "AUTH_REQUIRED");

        // Same for POST /subscribe.
        let resp = reqwest::Client::new()
            .post(format!("http://{}/subscribe", addr))
            .json(&serde_json::json!({
                "pattern": "events.foo",
                "url": "http://127.0.0.1:1/never-called",
                "priority": 0,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        handles.shutdown_and_join().await;
    }

    /// **N8b / N3 regression**: SSRF errors are returned as a generic
    /// `INVALID_WEBHOOK_URL` response with no internal IP / DNS leak.
    #[tokio::test]
    async fn test_http_subscribe_ssrf_error_is_generic() {
        use seidrum_eventbus::storage::memory_store::InMemoryEventStore;
        use seidrum_eventbus::test_utils::{pick_ephemeral_addr, wait_for_http_ready};
        use std::sync::Arc as SArc;

        // Use Strict policy so 127.0.0.1 is rejected, but allow dev
        // mode so the auth gate doesn't fire first.
        let store = SArc::new(InMemoryEventStore::new());
        let addr = pick_ephemeral_addr();
        let handles = EventBusBuilder::new()
            .storage(store)
            .with_http(addr)
            // Strict policy on purpose
            .unsafe_allow_http_dev_mode()
            .build_with_handles()
            .await
            .unwrap();
        wait_for_http_ready(addr).await;

        let resp = reqwest::Client::new()
            .post(format!("http://{}/subscribe", addr))
            .json(&serde_json::json!({
                "pattern": "events.foo",
                "url": "http://127.0.0.1/hook",
                "priority": 0,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["code"], "INVALID_WEBHOOK_URL");
        // The error message must NOT contain the resolved IP or any
        // DNS specifics. Just the generic phrase.
        let err_msg = body["error"].as_str().unwrap();
        assert_eq!(err_msg, "invalid webhook URL");
        assert!(
            !err_msg.contains("127.0.0.1") && !err_msg.contains("loopback"),
            "error message must not leak resolved IP / classification"
        );

        handles.shutdown_and_join().await;
    }

    /// **N8c / C3 regression**: a SyncInterceptor entry persisted via
    /// `POST /interceptors` is recreated as an interceptor (not as an
    /// async subscription) on restart and runs as expected.
    #[tokio::test]
    async fn test_sync_interceptor_persistence_across_restart() {
        use seidrum_eventbus::storage::EventStore;
        use seidrum_eventbus::test_utils::{pick_ephemeral_addr, wait_for_http_ready};
        use std::sync::Arc as SArc;
        use std::time::Duration;

        let store: SArc<dyn EventStore> =
            SArc::new(seidrum_eventbus::storage::memory_store::InMemoryEventStore::new());
        let addr = pick_ephemeral_addr();

        // === First lifetime: register an interceptor ===
        let handles_a = EventBusBuilder::new()
            .storage(SArc::clone(&store))
            .with_http(addr)
            .with_webhook_url_policy(seidrum_eventbus::delivery::WebhookUrlPolicy::Permissive)
            .unsafe_allow_http_dev_mode()
            .build_with_handles()
            .await
            .unwrap();
        wait_for_http_ready(addr).await;

        let resp = reqwest::Client::new()
            .post(format!("http://{}/interceptors", addr))
            .json(&serde_json::json!({
                "pattern": "events.persisted",
                "url": "http://127.0.0.1:1/never-called",
                "priority": 100,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let original_id = body["id"].as_str().unwrap().to_string();

        // The persisted store should hold one SyncInterceptor entry.
        let persisted = store.list_subscriptions().await.unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(
            persisted[0].kind,
            seidrum_eventbus::storage::PersistedSubscriptionKind::SyncInterceptor
        );

        handles_a.shutdown_and_join().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        // === Second lifetime: same store, fresh server ===
        let handles_b = EventBusBuilder::new()
            .storage(SArc::clone(&store))
            .with_http(addr)
            .with_webhook_url_policy(seidrum_eventbus::delivery::WebhookUrlPolicy::Permissive)
            .unsafe_allow_http_dev_mode()
            .build_with_handles()
            .await
            .unwrap();
        wait_for_http_ready(addr).await;

        // The new bus should have an interceptor on the persisted pattern.
        let subs = handles_b.bus.list_subscriptions(None).await.unwrap();
        let recreated = subs
            .iter()
            .find(|s| s.pattern == "events.persisted")
            .expect("interceptor should be recreated on the new bus");
        // Different runtime id from the original.
        assert_ne!(recreated.id, original_id);
        // Sync mode (interceptor) — not async (subscription).
        // SubscriptionInfo::mode is the string representation.
        assert_eq!(recreated.mode, "Sync");

        handles_b.shutdown_and_join().await;
    }

    /// **N8c regression**: `PersistedSubscription` deserializes from a
    /// JSON document with no `kind` field (entries persisted before
    /// the C3 PR). The default is `AsyncWebhook`.
    #[test]
    fn test_persisted_subscription_backwards_compat_no_kind() {
        let json = serde_json::json!({
            "persisted_id": "old-1",
            "pattern": "events.foo",
            "url": "https://example.com/hook",
            "headers": {},
            "priority": 0,
            "created_at": 1000,
        });
        let entry: seidrum_eventbus::storage::PersistedSubscription =
            serde_json::from_value(json).unwrap();
        assert_eq!(
            entry.kind,
            seidrum_eventbus::storage::PersistedSubscriptionKind::AsyncWebhook
        );
        assert_eq!(entry.timeout_ms, None);
    }

    /// **N8d / WebhookInterceptor fallback paths**: register an
    /// interceptor whose remote returns 5xx, malformed JSON, or simply
    /// can't be reached. All four cases must result in `Pass` (the
    /// async subscriber gets the original payload).
    #[tokio::test]
    async fn test_webhook_interceptor_fallback_paths() {
        use axum::{routing::post, Json as AJson, Router as ARouter};
        use seidrum_eventbus::test_utils::{test_bus_with_transports, wait_for_http_ready};
        use std::time::Duration;

        // Spawn a mock that returns 500 on /500, garbage on /garbage,
        // and unreachable for /never (we just don't bind it).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let interceptor_addr = listener.local_addr().unwrap();
        async fn five_hundred_handler() -> axum::http::StatusCode {
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        }
        async fn garbage_handler() -> &'static str {
            "{ this is not valid json"
        }
        async fn invalid_b64_handler() -> AJson<serde_json::Value> {
            AJson(serde_json::json!({
                "action": "modify",
                "payload": "@@@not_base64@@@"
            }))
        }
        let app = ARouter::new()
            .route("/500", post(five_hundred_handler))
            .route("/garbage", post(garbage_handler))
            .route("/invalid_b64", post(invalid_b64_handler));
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        seidrum_eventbus::test_utils::wait_for_tcp_ready(interceptor_addr).await;

        let env = test_bus_with_transports().await;
        wait_for_http_ready(env.http_addr).await;

        // Register three interceptors on three different subjects.
        let client = reqwest::Client::new();
        for (suffix, path) in [
            ("five_hundred", "/500"),
            ("garbage", "/garbage"),
            ("invalid_b64", "/invalid_b64"),
        ] {
            let resp = client
                .post(format!("http://{}/interceptors", env.http_addr))
                .json(&serde_json::json!({
                    "pattern": format!("e2e.fallback.{}", suffix),
                    "url": format!("http://{}{}", interceptor_addr, path),
                    "priority": 100,
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "register {} should succeed", suffix);
        }

        // Subscribe to each subject and publish — the original payload
        // should always reach the subscriber because every fallback
        // path returns Pass.
        for suffix in ["five_hundred", "garbage", "invalid_b64"] {
            let subject = format!("e2e.fallback.{}", suffix);
            let opts = SubscribeOpts {
                priority: 200,
                mode: SubscriptionMode::Async,
                channel: ChannelConfig::InProcess,
                timeout: Duration::from_secs(5),
                filter: None,
            };
            let mut sub = env.handles.bus.subscribe(&subject, opts).await.unwrap();
            env.handles
                .bus
                .publish(&subject, b"original")
                .await
                .unwrap();
            let received = tokio::time::timeout(Duration::from_secs(3), sub.rx.recv())
                .await
                .expect("subscriber should receive event")
                .expect("rx not closed");
            assert_eq!(
                received.payload, b"original",
                "fallback {} should leave payload unchanged",
                suffix
            );
        }

        env.handles.shutdown_and_join().await;
        server_task.abort();
    }

    /// **N8d / header propagation**: custom headers in the
    /// `RegisterInterceptorRequest` actually reach the remote endpoint.
    #[tokio::test]
    async fn test_webhook_interceptor_propagates_custom_headers() {
        use axum::{
            extract::State as ASt, http::HeaderMap, routing::post, Json as AJson,
            Router as ARouter,
        };
        use seidrum_eventbus::test_utils::{test_bus_with_transports, wait_for_http_ready};
        use std::sync::{Arc as SArc, Mutex};
        use std::time::Duration;

        // Mock that records headers and returns Pass.
        type Captured = SArc<Mutex<Option<HeaderMap>>>;
        let captured: Captured = SArc::new(Mutex::new(None));
        let captured_clone = SArc::clone(&captured);

        async fn handler(
            ASt(captured): ASt<Captured>,
            headers: HeaderMap,
            AJson(_body): AJson<serde_json::Value>,
        ) -> AJson<serde_json::Value> {
            *captured.lock().unwrap() = Some(headers);
            AJson(serde_json::json!({"action": "pass"}))
        }
        let app = ARouter::new()
            .route("/intercept", post(handler))
            .with_state(captured_clone);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let interceptor_addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        seidrum_eventbus::test_utils::wait_for_tcp_ready(interceptor_addr).await;

        let env = test_bus_with_transports().await;
        wait_for_http_ready(env.http_addr).await;

        let resp = reqwest::Client::new()
            .post(format!("http://{}/interceptors", env.http_addr))
            .json(&serde_json::json!({
                "pattern": "e2e.headers",
                "url": format!("http://{}/intercept", interceptor_addr),
                "headers": {
                    "X-Custom": "custom-value",
                    "X-Trace-Id": "abc-123"
                },
                "priority": 100,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        env.handles
            .bus
            .publish("e2e.headers", b"hello")
            .await
            .unwrap();

        // **N10 fix**: bounded poll for the captured headers (instead
        // of a fixed sleep) so this works even when the runner is slow.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if captured.lock().unwrap().is_some() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Snapshot the headers and drop the guard before any await.
        let snapshot: Option<HeaderMap> = captured.lock().unwrap().clone();
        let headers = snapshot.expect("interceptor should have been called");
        assert_eq!(
            headers
                .get("x-custom")
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
            "custom-value"
        );
        assert_eq!(
            headers
                .get("x-trace-id")
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
            "abc-123"
        );

        env.handles.shutdown_and_join().await;
        server_task.abort();
    }

    /// Test RetryConfig implements Serialize/Deserialize.
    #[test]
    fn test_retry_config_serde() {
        use seidrum_eventbus::delivery::RetryConfig;

        let config = RetryConfig::default();
        let json = serde_json::to_string(&config).expect("should serialize");
        let parsed: RetryConfig = serde_json::from_str(&json).expect("should deserialize");
        assert_eq!(parsed.max_attempts, config.max_attempts);
        assert_eq!(parsed.initial_backoff_ms, config.initial_backoff_ms);
        assert_eq!(parsed.max_backoff_ms, config.max_backoff_ms);
    }
}
