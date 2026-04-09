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

        // Subtractive jitter: result is always in [base - base/4, base]
        let d1 = calculate_backoff(0, 100, 30000);
        assert!(
            d1.as_millis() >= 75 && d1.as_millis() <= 100,
            "attempt 0: expected 75..100ms, got {}ms",
            d1.as_millis()
        );

        let d2 = calculate_backoff(1, 100, 30000);
        assert!(
            d2.as_millis() >= 150 && d2.as_millis() <= 200,
            "attempt 1: expected 150..200ms, got {}ms",
            d2.as_millis()
        );

        let d5 = calculate_backoff(5, 100, 30000);
        // 2^5 * 100 = 3200, jitter removes up to 800 → 2400..3200
        assert!(
            d5.as_millis() >= 2400 && d5.as_millis() <= 3200,
            "attempt 5: expected 2400..3200ms, got {}ms",
            d5.as_millis()
        );

        let d10 = calculate_backoff(10, 100, 30000);
        assert!(
            d10.as_millis() <= 30000,
            "attempt 10 should be capped at 30000ms, got {}ms",
            d10.as_millis()
        );
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
