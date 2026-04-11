//! Proxy [`Interceptor`] that forwards intercept calls over a WebSocket connection.
//!
//! Mirrors the design of [`crate::delivery::WsRemoteChannel`]: when a remote
//! WebSocket client sends a `register_interceptor` operation, the WS server
//! installs an instance of [`WsRemoteInterceptor`] into the bus's interceptor
//! chain via [`crate::EventBus::intercept`]. Subsequent events whose subjects
//! match the registered pattern are dispatched to the remote client over the
//! existing WS connection, and the remote responds with a Pass/Modify/Drop
//! decision.
//!
//! # Wire protocol
//!
//! Server → client: `{"op":"intercept","request_id":"...","subject":"...","payload":"<b64>"}`
//!
//! Client → server: `{"op":"intercept_result","request_id":"...","action":"pass"|"modify"|"drop","payload":"<b64>?"}`
//!
//! # Timeout behaviour
//!
//! If the client does not respond within `intercept_timeout`, the interceptor
//! returns [`InterceptResult::Pass`] so the in-process dispatch chain
//! continues. This matches the spec's "sync interceptors that exceed their
//! timeout are skipped" rule and avoids silently dropping events when a
//! remote handler is slow.
//!
//! # Disconnect behaviour
//!
//! When the WS connection drops, the connection's cleanup code calls
//! [`WsRemoteInterceptor::cancel_all`], which fails any in-flight
//! `intercept()` calls with a `Pass` result so they don't hang the
//! dispatch loop.

use crate::dispatch::{InterceptResult, Interceptor};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, warn};

/// A serialized outbound frame to be sent to the WebSocket client.
/// Reuses the same `String` type that [`crate::delivery::WsRemoteChannel`]
/// pushes onto the connection's outbound queue.
pub type WsOutboundFrame = String;

/// Action returned by the remote client in response to an `intercept`
/// frame. Mirrors [`InterceptResult`] over the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsInterceptAction {
    Pass,
    Modify(Vec<u8>),
    Drop,
}

/// Reply signaled from the WS connection's read loop back to the awaiting
/// `intercept()` call. Wraps the action plus an optional error message.
#[derive(Debug)]
pub struct WsInterceptReply {
    pub action: WsInterceptAction,
    /// Set when the connection drops or the read loop cancels pending
    /// replies before the client responded.
    pub error: Option<String>,
}

/// Remote interceptor proxy.
///
/// Holds the outbound queue (shared with [`crate::delivery::WsRemoteChannel`]
/// instances on the same connection) and a per-instance pending-replies map.
pub struct WsRemoteInterceptor {
    /// Pattern this interceptor was registered under (for logging).
    pattern: String,
    /// Outbound frame queue. The connection's writer task drains this and
    /// writes frames to the WebSocket. Cloned from `forward_tx` in the
    /// connection handler.
    outbound: mpsc::Sender<WsOutboundFrame>,
    /// Pending intercept replies, keyed by request id.
    pending_replies: Arc<Mutex<HashMap<String, oneshot::Sender<WsInterceptReply>>>>,
    /// Per-call timeout. If the remote client doesn't respond in this
    /// window, the interceptor returns `Pass` and logs a warning.
    intercept_timeout: Duration,
}

impl WsRemoteInterceptor {
    /// Create a new remote interceptor proxy.
    pub fn new(pattern: String, outbound: mpsc::Sender<WsOutboundFrame>) -> Self {
        Self {
            pattern,
            outbound,
            pending_replies: Arc::new(Mutex::new(HashMap::new())),
            intercept_timeout: Duration::from_secs(5),
        }
    }

    /// Override the per-call timeout. Default 5 seconds.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.intercept_timeout = timeout;
        self
    }

    /// Get a handle to the pending replies map. The WS connection's read
    /// loop uses this to signal `intercept_result` messages back to
    /// awaiting `intercept()` calls.
    pub fn pending_replies(
        &self,
    ) -> Arc<Mutex<HashMap<String, oneshot::Sender<WsInterceptReply>>>> {
        Arc::clone(&self.pending_replies)
    }

    /// Cancel all pending intercept calls with a "pass" result so the
    /// dispatch loop continues. Called when the connection drops.
    pub async fn cancel_all(&self, reason: &str) {
        let mut pending = self.pending_replies.lock().await;
        let count = pending.len();
        for (_, tx) in pending.drain() {
            let _ = tx.send(WsInterceptReply {
                action: WsInterceptAction::Pass,
                error: Some(reason.to_string()),
            });
        }
        if count > 0 {
            debug!(
                pattern = %self.pattern,
                cancelled = count,
                reason = reason,
                "WsRemoteInterceptor cancelled pending intercept calls"
            );
        }
    }
}

#[async_trait]
impl Interceptor for WsRemoteInterceptor {
    async fn intercept(&self, subject: &str, payload: &mut Vec<u8>) -> InterceptResult {
        let request_id = ulid::Ulid::new().to_string();

        // Register a oneshot to receive the client's reply.
        let (tx, rx) = oneshot::channel::<WsInterceptReply>();
        {
            let mut pending = self.pending_replies.lock().await;
            pending.insert(request_id.clone(), tx);
        }

        // Build the intercept frame and push it to the outbound queue.
        use base64::Engine;
        let payload_b64 = base64::engine::general_purpose::STANDARD.encode(payload.as_slice());
        let frame = serde_json::json!({
            "op": "intercept",
            "request_id": request_id,
            "subject": subject,
            "payload": payload_b64,
        });
        let frame_str = match serde_json::to_string(&frame) {
            Ok(s) => s,
            Err(e) => {
                self.pending_replies.lock().await.remove(&request_id);
                warn!(
                    pattern = %self.pattern,
                    error = %e,
                    "WsRemoteInterceptor failed to encode frame; passing event through"
                );
                return InterceptResult::Pass;
            }
        };

        if self.outbound.send(frame_str).await.is_err() {
            // Outbound queue closed — connection is gone.
            self.pending_replies.lock().await.remove(&request_id);
            warn!(
                pattern = %self.pattern,
                "WsRemoteInterceptor outbound closed; passing event through"
            );
            return InterceptResult::Pass;
        }

        // Wait for the client's reply with a timeout.
        let reply = match tokio::time::timeout(self.intercept_timeout, rx).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => {
                // oneshot sender dropped without a value — typically
                // because the connection dropped concurrently.
                warn!(
                    pattern = %self.pattern,
                    "WsRemoteInterceptor reply channel closed; passing event through"
                );
                return InterceptResult::Pass;
            }
            Err(_) => {
                // Timed out. Clean up the pending entry so a late reply
                // doesn't leak the slot.
                self.pending_replies.lock().await.remove(&request_id);
                warn!(
                    pattern = %self.pattern,
                    request_id = request_id,
                    "WsRemoteInterceptor timed out; passing event through"
                );
                return InterceptResult::Pass;
            }
        };

        // Translate the wire-level action into the in-process result.
        // For Modified, we swap the new payload into the caller's buffer.
        match reply.action {
            WsInterceptAction::Pass => InterceptResult::Pass,
            WsInterceptAction::Drop => InterceptResult::Drop,
            WsInterceptAction::Modify(new_payload) => {
                *payload = new_payload;
                InterceptResult::Modified
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_intercept_pass_round_trip() {
        let (tx, mut rx) = mpsc::channel::<WsOutboundFrame>(8);
        let interceptor = Arc::new(WsRemoteInterceptor::new("test.pass".to_string(), tx));

        // Simulate the WS read loop replying with Pass.
        let pending = interceptor.pending_replies();
        tokio::spawn(async move {
            let frame = rx.recv().await.unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&frame).unwrap();
            let request_id = parsed["request_id"].as_str().unwrap().to_string();
            assert_eq!(parsed["op"], "intercept");
            assert_eq!(parsed["subject"], "test.pass");

            let mut p = pending.lock().await;
            if let Some(reply_tx) = p.remove(&request_id) {
                let _ = reply_tx.send(WsInterceptReply {
                    action: WsInterceptAction::Pass,
                    error: None,
                });
            }
        });

        let mut payload = b"hello".to_vec();
        let result = interceptor.intercept("test.pass", &mut payload).await;
        assert_eq!(result, InterceptResult::Pass);
        assert_eq!(payload, b"hello"); // unchanged
    }

    #[tokio::test]
    async fn test_intercept_modify_swaps_payload() {
        let (tx, mut rx) = mpsc::channel::<WsOutboundFrame>(8);
        let interceptor = Arc::new(WsRemoteInterceptor::new("test.modify".to_string(), tx));

        let pending = interceptor.pending_replies();
        tokio::spawn(async move {
            let frame = rx.recv().await.unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&frame).unwrap();
            let request_id = parsed["request_id"].as_str().unwrap().to_string();

            let mut p = pending.lock().await;
            if let Some(reply_tx) = p.remove(&request_id) {
                let _ = reply_tx.send(WsInterceptReply {
                    action: WsInterceptAction::Modify(b"replaced!".to_vec()),
                    error: None,
                });
            }
        });

        let mut payload = b"original".to_vec();
        let result = interceptor.intercept("test.modify", &mut payload).await;
        assert_eq!(result, InterceptResult::Modified);
        assert_eq!(payload, b"replaced!");
    }

    #[tokio::test]
    async fn test_intercept_drop_propagates() {
        let (tx, mut rx) = mpsc::channel::<WsOutboundFrame>(8);
        let interceptor = Arc::new(WsRemoteInterceptor::new("test.drop".to_string(), tx));

        let pending = interceptor.pending_replies();
        tokio::spawn(async move {
            let frame = rx.recv().await.unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&frame).unwrap();
            let request_id = parsed["request_id"].as_str().unwrap().to_string();

            let mut p = pending.lock().await;
            if let Some(reply_tx) = p.remove(&request_id) {
                let _ = reply_tx.send(WsInterceptReply {
                    action: WsInterceptAction::Drop,
                    error: None,
                });
            }
        });

        let mut payload = b"toxic".to_vec();
        let result = interceptor.intercept("test.drop", &mut payload).await;
        assert_eq!(result, InterceptResult::Drop);
    }

    #[tokio::test]
    async fn test_intercept_timeout_passes_through() {
        let (tx, _rx) = mpsc::channel::<WsOutboundFrame>(8);
        let interceptor = WsRemoteInterceptor::new("test.timeout".to_string(), tx)
            .with_timeout(Duration::from_millis(50));

        // No task to handle the reply — should time out and Pass.
        let mut payload = b"x".to_vec();
        let result = interceptor.intercept("test.timeout", &mut payload).await;
        assert_eq!(result, InterceptResult::Pass);
        // Payload preserved.
        assert_eq!(payload, b"x");
    }

    #[tokio::test]
    async fn test_cancel_all_returns_pass() {
        let (tx, _rx) = mpsc::channel::<WsOutboundFrame>(8);
        let interceptor = Arc::new(WsRemoteInterceptor::new("test.cancel".to_string(), tx));

        let interceptor_clone = Arc::clone(&interceptor);
        let task = tokio::spawn(async move {
            let mut p = b"x".to_vec();
            interceptor_clone.intercept("test.cancel", &mut p).await
        });

        // Give the intercept call time to register its pending reply.
        tokio::time::sleep(Duration::from_millis(20)).await;

        interceptor.cancel_all("disconnect test").await;

        let result = task.await.unwrap();
        assert_eq!(result, InterceptResult::Pass);
    }

    #[tokio::test]
    async fn test_outbound_closed_passes_through() {
        let (tx, rx) = mpsc::channel::<WsOutboundFrame>(8);
        let interceptor = WsRemoteInterceptor::new("test.closed".to_string(), tx);

        // Drop the receiver so the next send() fails.
        drop(rx);

        let mut payload = b"x".to_vec();
        let result = interceptor.intercept("test.closed", &mut payload).await;
        assert_eq!(result, InterceptResult::Pass);
    }
}
