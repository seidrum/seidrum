//! Proxy delivery channel that forwards events over a WebSocket connection.
//!
//! When a remote WebSocket client registers a custom channel type via the
//! `register_channel_type` protocol message, the server installs an
//! instance of [`WsRemoteChannel`] in the bus's
//! [`crate::delivery::ChannelRegistry`]. Subsequent subscriptions that use
//! `ChannelConfig::Custom { channel_type, .. }` matching the registered
//! type will route their events through this proxy.
//!
//! On `deliver()`:
//! 1. The proxy generates a request id and registers an oneshot reply
//!    channel under that id in its `pending_replies` map.
//! 2. It serializes a `Deliver` message and sends it to the connection's
//!    outbound channel.
//! 3. It awaits the oneshot reply (with a timeout).
//! 4. The connection's read loop matches incoming `DeliverResult` messages
//!    by `request_id` and signals the appropriate oneshot.
//!
//! On client disconnect, the connection's drop handler signals all pending
//! replies with an error so in-flight deliveries fail and enter the retry
//! queue.

use super::{ChannelConfig, DeliveryChannel, DeliveryError, DeliveryReceipt, DeliveryResult};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, warn};

/// A serialized outbound frame to be sent to the WebSocket client.
/// The transport layer owns the actual WebSocket sender; this proxy only
/// pushes JSON strings into a channel.
pub type WsOutboundFrame = String;

/// Result of a remote delivery attempt, signaled via oneshot from the
/// connection's read loop back to the awaiting `deliver()` call.
#[derive(Debug)]
pub struct WsDeliveryReply {
    pub success: bool,
    pub error: Option<String>,
}

/// Proxy delivery channel backed by a WebSocket connection.
///
/// Instances are created by the WS transport server when a client sends a
/// `register_channel_type` operation. Each instance owns:
/// - The outbound channel for sending frames to the WS client
/// - A pending-replies map keyed by request id
///
/// Cloneable via `Arc` because [`crate::delivery::ChannelRegistry`] stores
/// channels behind `Arc<dyn DeliveryChannel>`.
pub struct WsRemoteChannel {
    /// Channel type name (for logging / debugging).
    channel_type: String,
    /// Outbound frame queue. The connection's writer task drains this and
    /// writes frames to the WebSocket.
    outbound: mpsc::Sender<WsOutboundFrame>,
    /// Pending delivery replies, keyed by request id.
    pending_replies: Arc<Mutex<HashMap<String, oneshot::Sender<WsDeliveryReply>>>>,
    /// Per-delivery timeout. If the client doesn't respond in this window
    /// the delivery is recorded as Failed and retried.
    delivery_timeout: Duration,
}

impl WsRemoteChannel {
    /// Create a new remote channel proxy.
    ///
    /// `outbound` is the connection's outbound frame queue (typically the
    /// `forward_tx` mpsc sender already used by the WS connection handler).
    pub fn new(channel_type: String, outbound: mpsc::Sender<WsOutboundFrame>) -> Self {
        Self {
            channel_type,
            outbound,
            pending_replies: Arc::new(Mutex::new(HashMap::new())),
            delivery_timeout: Duration::from_secs(30),
        }
    }

    /// Get a handle to the pending replies map. The connection's read loop
    /// uses this to signal `DeliverResult` messages back to awaiting
    /// `deliver()` calls.
    pub fn pending_replies(&self) -> Arc<Mutex<HashMap<String, oneshot::Sender<WsDeliveryReply>>>> {
        Arc::clone(&self.pending_replies)
    }

    /// Set the per-delivery timeout. Default 30s.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.delivery_timeout = timeout;
        self
    }

    /// Cancel all pending deliveries with an error. Called when the
    /// connection drops so awaiting `deliver()` calls fail promptly and
    /// the failed deliveries enter the retry queue.
    pub async fn cancel_all(&self, reason: &str) {
        let mut pending = self.pending_replies.lock().await;
        let count = pending.len();
        for (_, tx) in pending.drain() {
            let _ = tx.send(WsDeliveryReply {
                success: false,
                error: Some(reason.to_string()),
            });
        }
        if count > 0 {
            debug!(
                channel_type = %self.channel_type,
                cancelled = count,
                reason = reason,
                "WsRemoteChannel cancelled pending deliveries"
            );
        }
    }
}

#[async_trait]
impl DeliveryChannel for WsRemoteChannel {
    async fn deliver(
        &self,
        event: &[u8],
        subject: &str,
        _config: &ChannelConfig,
    ) -> DeliveryResult<DeliveryReceipt> {
        let start = SystemTime::now();
        let request_id = ulid::Ulid::new().to_string();

        // Register a oneshot to receive the client's reply.
        let (tx, rx) = oneshot::channel::<WsDeliveryReply>();
        {
            let mut pending = self.pending_replies.lock().await;
            pending.insert(request_id.clone(), tx);
        }

        // Build the deliver frame and push it to the outbound queue.
        use base64::Engine;
        let payload_b64 = base64::engine::general_purpose::STANDARD.encode(event);
        let frame = serde_json::json!({
            "op": "deliver",
            "request_id": request_id,
            "channel_type": self.channel_type,
            "subject": subject,
            "payload": payload_b64,
        });
        let frame_str = match serde_json::to_string(&frame) {
            Ok(s) => s,
            Err(e) => {
                self.pending_replies.lock().await.remove(&request_id);
                return Err(DeliveryError::Failed(format!(
                    "encode deliver frame: {}",
                    e
                )));
            }
        };

        if self.outbound.send(frame_str).await.is_err() {
            // Outbound queue closed — connection is gone.
            self.pending_replies.lock().await.remove(&request_id);
            return Err(DeliveryError::NotReady);
        }

        // Wait for the client's reply with a timeout.
        let reply = match tokio::time::timeout(self.delivery_timeout, rx).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => {
                // oneshot sender dropped without a value — typically
                // because the connection dropped.
                return Err(DeliveryError::NotReady);
            }
            Err(_) => {
                // Timed out waiting for the client. Clean up the pending
                // entry so a late reply doesn't leak the slot.
                self.pending_replies.lock().await.remove(&request_id);
                warn!(
                    channel_type = %self.channel_type,
                    request_id = request_id,
                    "WsRemoteChannel delivery timed out"
                );
                return Err(DeliveryError::Failed(format!(
                    "remote delivery timed out after {:?}",
                    self.delivery_timeout
                )));
            }
        };

        if !reply.success {
            return Err(DeliveryError::Failed(
                reply
                    .error
                    .unwrap_or_else(|| "remote channel reported failure".to_string()),
            ));
        }

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
        self.cancel_all("channel cleanup").await;
        Ok(())
    }

    async fn is_healthy(&self, _config: &ChannelConfig) -> bool {
        // The outbound channel is closed when the WS connection drops.
        !self.outbound.is_closed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_deliver_success_round_trip() {
        let (tx, mut rx) = mpsc::channel::<WsOutboundFrame>(8);
        let channel = Arc::new(WsRemoteChannel::new("test_channel".to_string(), tx));

        // Spawn a task that simulates the WS client: read the deliver
        // frame, signal success on the corresponding pending reply.
        let pending = channel.pending_replies();
        tokio::spawn(async move {
            let frame = rx.recv().await.unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&frame).unwrap();
            let request_id = parsed["request_id"].as_str().unwrap().to_string();

            // Simulate the WS read loop signaling the reply.
            let mut p = pending.lock().await;
            if let Some(reply_tx) = p.remove(&request_id) {
                let _ = reply_tx.send(WsDeliveryReply {
                    success: true,
                    error: None,
                });
            }
        });

        let result = channel
            .deliver(b"hello", "test.subject", &ChannelConfig::InProcess)
            .await;
        assert!(result.is_ok(), "deliver should succeed: {:?}", result);
    }

    #[tokio::test]
    async fn test_deliver_failure_propagates() {
        let (tx, mut rx) = mpsc::channel::<WsOutboundFrame>(8);
        let channel = Arc::new(WsRemoteChannel::new("test_channel".to_string(), tx));

        let pending = channel.pending_replies();
        tokio::spawn(async move {
            let frame = rx.recv().await.unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&frame).unwrap();
            let request_id = parsed["request_id"].as_str().unwrap().to_string();

            let mut p = pending.lock().await;
            if let Some(reply_tx) = p.remove(&request_id) {
                let _ = reply_tx.send(WsDeliveryReply {
                    success: false,
                    error: Some("client rejected".to_string()),
                });
            }
        });

        let result = channel
            .deliver(b"x", "test.subject", &ChannelConfig::InProcess)
            .await;
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("client rejected"), "got: {}", err);
    }

    #[tokio::test]
    async fn test_deliver_timeout() {
        let (tx, _rx) = mpsc::channel::<WsOutboundFrame>(8);
        let channel = WsRemoteChannel::new("test_channel".to_string(), tx)
            .with_timeout(Duration::from_millis(50));

        // No task to handle the reply — should time out.
        let result = channel
            .deliver(b"x", "test.subject", &ChannelConfig::InProcess)
            .await;
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("timed out"), "got: {}", err);
    }

    #[tokio::test]
    async fn test_cancel_all_on_disconnect() {
        let (tx, _rx) = mpsc::channel::<WsOutboundFrame>(8);
        let channel = Arc::new(WsRemoteChannel::new("test_channel".to_string(), tx));

        // Start a deliver but don't process it.
        let channel_clone = Arc::clone(&channel);
        let deliver_task = tokio::spawn(async move {
            channel_clone
                .deliver(b"x", "test.subject", &ChannelConfig::InProcess)
                .await
        });

        // Give the deliver call time to register its pending reply.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Cancel all — simulates a disconnect.
        channel.cancel_all("disconnect test").await;

        let result = deliver_task.await.unwrap();
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("disconnect"), "got: {}", err);
    }

    #[tokio::test]
    async fn test_is_healthy_reflects_outbound_state() {
        let (tx, rx) = mpsc::channel::<WsOutboundFrame>(8);
        let channel = WsRemoteChannel::new("t".to_string(), tx);
        assert!(channel.is_healthy(&ChannelConfig::InProcess).await);
        drop(rx);
        assert!(!channel.is_healthy(&ChannelConfig::InProcess).await);
    }
}
