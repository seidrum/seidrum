//! Standalone WebSocket delivery channel.
//!
//! Delivers events over a WebSocket connection by writing JSON frames.
//!
//! This is a **standalone delivery channel** for pushing events to externally
//! managed WebSocket connections (e.g., when embedding the bus in your own
//! server). It is separate from [`crate::transport::ws::WebSocketServer`],
//! which manages its own connections and forwarding internally.

use super::{DeliveryChannel, DeliveryError, DeliveryReceipt, DeliveryResult};
use crate::delivery::ChannelConfig;
use async_trait::async_trait;
use base64::Engine;
use serde_json::json;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::debug;

/// Message sent to a WebSocket connection for delivery.
pub struct WebSocketMessage {
    pub payload: Vec<u8>,
}

/// Wrapper around a WebSocket sender channel.
pub struct WebSocketChannel {
    tx: mpsc::UnboundedSender<WebSocketMessage>,
}

impl WebSocketChannel {
    /// Create a new WebSocket delivery channel with a sender.
    pub fn new(tx: mpsc::UnboundedSender<WebSocketMessage>) -> Arc<Self> {
        Arc::new(Self { tx })
    }
}

#[async_trait]
impl DeliveryChannel for WebSocketChannel {
    async fn deliver(
        &self,
        event: &[u8],
        subject: &str,
        _config: &ChannelConfig,
    ) -> DeliveryResult<DeliveryReceipt> {
        let start = SystemTime::now();

        // Build the event message
        let payload_b64 = base64::engine::general_purpose::STANDARD.encode(event);
        let message = json!({
            "op": "event",
            "subject": subject,
            "payload": payload_b64,
        });

        let msg_bytes = serde_json::to_vec(&message)
            .map_err(|e| DeliveryError::Failed(format!("JSON encode error: {}", e)))?;

        self.tx
            .send(WebSocketMessage { payload: msg_bytes })
            .map_err(|_| DeliveryError::Failed("WebSocket channel closed".to_string()))?;

        let elapsed = SystemTime::now()
            .duration_since(start)
            .unwrap_or_default()
            .as_micros() as u64;

        let delivered_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        Ok(DeliveryReceipt {
            delivered_at,
            latency_us: elapsed,
        })
    }

    async fn cleanup(&self, _config: &ChannelConfig) -> DeliveryResult<()> {
        debug!("WebSocket channel cleanup");
        Ok(())
    }

    async fn is_healthy(&self, _config: &ChannelConfig) -> bool {
        !self.tx.is_closed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_websocket_delivery() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let channel = WebSocketChannel::new(tx);

        let event = b"test event";
        let result = channel
            .deliver(event, "test.subject", &ChannelConfig::InProcess)
            .await;

        assert!(result.is_ok());
        let receipt = result.unwrap();
        assert!(receipt.latency_us < 10000); // Should be very fast

        let msg = rx.recv().await;
        assert!(msg.is_some());
    }

    #[tokio::test]
    async fn test_websocket_closed_channel() {
        let (tx, _rx) = mpsc::unbounded_channel::<WebSocketMessage>();
        drop(_rx); // Close the receiver
        let channel = WebSocketChannel::new(tx);

        let event = b"test event";
        let result = channel
            .deliver(event, "test.subject", &ChannelConfig::InProcess)
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_websocket_is_healthy() {
        let (tx, rx) = mpsc::unbounded_channel();
        let channel = WebSocketChannel::new(tx);

        assert!(channel.is_healthy(&ChannelConfig::InProcess).await);

        drop(rx);
        assert!(!channel.is_healthy(&ChannelConfig::InProcess).await);
    }
}
