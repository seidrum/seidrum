//! Standalone in-process [`DeliveryChannel`] backed by a tokio mpsc channel.
//!
//! This is a self-contained delivery channel that can be used by callers
//! who want to embed the [`DeliveryChannel`] trait API in their own
//! pipelines. It is **not** used by the dispatch engine itself — the
//! engine routes in-process events through bounded mpsc channels owned by
//! [`crate::bus::Subscription`] handles.

use super::{ChannelConfig, DeliveryChannel, DeliveryError, DeliveryReceipt, DeliveryResult};
use async_trait::async_trait;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::{self, UnboundedReceiver};

/// A standalone in-process delivery channel.
///
/// Wraps an unbounded tokio mpsc sender. Use [`InProcessChannel::new`] to
/// construct a paired sender + receiver.
pub struct InProcessChannel {
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl InProcessChannel {
    /// Create a new in-process channel pair.
    /// Returns the channel (for `deliver()` calls) and the receiver
    /// the consumer reads from.
    pub fn new() -> (Self, UnboundedReceiver<Vec<u8>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }
}

#[async_trait]
impl DeliveryChannel for InProcessChannel {
    async fn deliver(
        &self,
        event: &[u8],
        _subject: &str,
        _config: &ChannelConfig,
    ) -> DeliveryResult<DeliveryReceipt> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        self.tx
            .send(event.to_vec())
            .map_err(|_| DeliveryError::NotReady)?;

        Ok(DeliveryReceipt {
            delivered_at: now,
            latency_us: 1, // approximately instantaneous
        })
    }

    async fn cleanup(&self, _config: &ChannelConfig) -> DeliveryResult<()> {
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
    async fn test_in_process_delivery() {
        let (channel, mut rx) = InProcessChannel::new();
        let payload = b"test message";

        let receipt = channel
            .deliver(payload, "test.subject", &ChannelConfig::InProcess)
            .await
            .unwrap();

        assert!(receipt.latency_us > 0);
        let received = rx.recv().await.unwrap();
        assert_eq!(received, payload);
    }

    #[tokio::test]
    async fn test_in_process_multiple_deliveries() {
        let (channel, mut rx) = InProcessChannel::new();

        for i in 0..5 {
            let payload = format!("message {}", i).into_bytes();
            channel
                .deliver(&payload, "test", &ChannelConfig::InProcess)
                .await
                .unwrap();
        }

        for i in 0..5 {
            let received = rx.recv().await.unwrap();
            assert_eq!(received, format!("message {}", i).into_bytes());
        }
    }

    #[tokio::test]
    async fn test_in_process_health() {
        let (channel, _rx) = InProcessChannel::new();
        assert!(channel.is_healthy(&ChannelConfig::InProcess).await);
    }
}
