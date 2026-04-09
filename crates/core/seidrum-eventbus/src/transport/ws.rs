//! WebSocket transport server.
//!
//! Provides a WebSocket server that allows remote clients to connect
//! and interact with the event bus using a JSON protocol.
//!
//! ## Protocol
//!
//! Clients send JSON messages with an `"op"` field identifying the operation.
//! The server responds with JSON messages containing results or forwarded events.
//!
//! ## Security
//!
//! Authentication is handled via the [`Authenticator`] trait. Provide an implementation
//! to the server to validate connections. Without an authenticator, all connections
//! are accepted (suitable for development only).

use crate::bus::{EventBus, SubscribeOpts};
use crate::delivery::ChannelConfig;
use crate::dispatch::SubscriptionMode;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use ulid::Ulid;

/// Maximum number of subscriptions a single WebSocket connection may hold.
const MAX_SUBSCRIPTIONS_PER_CONNECTION: usize = 64;

/// Maximum payload size in bytes before base64 decoding (1 MiB encoded ≈ 768 KiB decoded).
/// This limit applies to the base64-encoded string in the JSON message.
const MAX_PAYLOAD_SIZE: usize = 1_048_576;

/// Maximum size of a single incoming WebSocket text frame (2 MiB).
const MAX_FRAME_SIZE: usize = 2_097_152;

/// Authentication trait for WebSocket connections.
///
/// Implement this to add authentication to the WebSocket transport server.
/// The authenticator is called once per connection during the upgrade handshake.
#[async_trait::async_trait]
pub trait Authenticator: Send + Sync + 'static {
    /// Authenticate a connection from the given peer address.
    /// Returns `Ok(())` if allowed, or `Err(reason)` if rejected.
    async fn authenticate(&self, peer_addr: SocketAddr) -> Result<(), String>;
}

/// No-op authenticator that accepts all connections (development only).
pub struct NoAuth;

#[async_trait::async_trait]
impl Authenticator for NoAuth {
    async fn authenticate(&self, _peer_addr: SocketAddr) -> Result<(), String> {
        Ok(())
    }
}

/// WebSocket protocol operations sent by clients.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op")]
#[serde(rename_all = "lowercase")]
pub enum ClientOperation {
    /// Publish an event.
    #[serde(rename = "publish")]
    Publish {
        subject: String,
        /// Base64-encoded payload.
        payload: String,
        /// Optional correlation ID for request tracing.
        #[serde(default)]
        correlation_id: Option<String>,
    },
    /// Subscribe to a subject pattern.
    #[serde(rename = "subscribe")]
    Subscribe {
        pattern: String,
        #[serde(default)]
        opts: SubscribeOptions,
        #[serde(default)]
        correlation_id: Option<String>,
    },
    /// Unsubscribe from a subscription.
    #[serde(rename = "unsubscribe")]
    Unsubscribe {
        id: String,
        #[serde(default)]
        correlation_id: Option<String>,
    },
    /// Send a request and wait for a reply.
    #[serde(rename = "request")]
    Request {
        subject: String,
        /// Base64-encoded payload.
        payload: String,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
        #[serde(default)]
        correlation_id: Option<String>,
    },
}

fn default_timeout_ms() -> u64 {
    5000
}

/// Subscribe options from client.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct SubscribeOptions {
    #[serde(default)]
    pub priority: u32,
}

/// Outbound message from server to client.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op")]
#[serde(rename_all = "lowercase")]
pub enum ServerMessage {
    /// Event delivered to subscriber.
    #[serde(rename = "event")]
    Event {
        subject: String,
        /// Base64-encoded payload.
        payload: String,
        reply_subject: Option<String>,
        subscription_id: String,
    },
    /// Reply to a request operation.
    #[serde(rename = "reply_result")]
    ReplyResult {
        /// Base64-encoded payload.
        payload: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        correlation_id: Option<String>,
    },
    /// Confirmation of a successful subscribe.
    #[serde(rename = "subscribed")]
    Subscribed {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        correlation_id: Option<String>,
    },
    /// Error message.
    #[serde(rename = "error")]
    Error {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        correlation_id: Option<String>,
    },
}

/// WebSocket server for remote event bus clients.
///
/// Call [`WebSocketServer::start`] to begin accepting connections. The server
/// runs until an error occurs or the task is cancelled.
pub struct WebSocketServer {
    bus: Arc<dyn EventBus>,
    authenticator: Arc<dyn Authenticator>,
}

impl WebSocketServer {
    /// Create a new WebSocket server with no authentication (development only).
    pub fn new(bus: Arc<dyn EventBus>) -> Self {
        Self {
            bus,
            authenticator: Arc::new(NoAuth),
        }
    }

    /// Create a new WebSocket server with a custom authenticator.
    pub fn with_auth(bus: Arc<dyn EventBus>, auth: Arc<dyn Authenticator>) -> Self {
        Self {
            bus,
            authenticator: auth,
        }
    }

    /// Start the WebSocket server on the given address.
    ///
    /// This method runs indefinitely, accepting connections and spawning a handler
    /// task for each one. Returns `Err` only if the initial bind fails.
    pub async fn start(&self, addr: SocketAddr) -> crate::Result<()> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| crate::EventBusError::Internal(format!("Failed to bind: {}", e)))?;

        info!("WebSocket server listening on {}", addr);

        loop {
            let (socket, peer_addr) = listener
                .accept()
                .await
                .map_err(|e| crate::EventBusError::Internal(format!("Accept failed: {}", e)))?;

            let bus = Arc::clone(&self.bus);
            let auth = Arc::clone(&self.authenticator);
            tokio::spawn(async move {
                // Authenticate before upgrading
                if let Err(reason) = auth.authenticate(peer_addr).await {
                    warn!(
                        "WebSocket connection rejected from {}: {}",
                        peer_addr, reason
                    );
                    return;
                }

                if let Err(e) = handle_connection(socket, peer_addr, bus).await {
                    warn!(
                        "Error handling WebSocket connection from {}: {}",
                        peer_addr, e
                    );
                }
            });
        }
    }
}

/// Per-connection subscription bookkeeping.
struct ConnectionSubscription {
    pattern: String,
}

async fn handle_connection(
    socket: TcpStream,
    peer_addr: std::net::SocketAddr,
    bus: Arc<dyn EventBus>,
) -> crate::Result<()> {
    debug!("WebSocket client connected: {}", peer_addr);

    let ws = tokio_tungstenite::accept_async(socket)
        .await
        .map_err(|e| crate::EventBusError::Internal(format!("WebSocket upgrade failed: {}", e)))?;

    let (mut sender, mut receiver) = ws.split();
    let connection_id = Ulid::new().to_string();
    let mut subscriptions: HashMap<String, ConnectionSubscription> = HashMap::new();

    // Channel for forwarding subscription events to the WebSocket sender.
    // Subscription receivers are polled in a separate task and forwarded here.
    let (forward_tx, mut forward_rx) = mpsc::channel::<String>(256);

    loop {
        tokio::select! {
            // Incoming WebSocket messages from the client
            msg_opt = receiver.next() => {
                let msg = match msg_opt {
                    Some(Ok(msg)) => msg,
                    Some(Err(e)) => {
                        debug!("WebSocket error from {}: {}", peer_addr, e);
                        break;
                    }
                    None => break, // Connection closed
                };

                if msg.is_close() {
                    break;
                }

                if !msg.is_text() {
                    continue;
                }

                let text = match msg.to_text() {
                    Ok(t) => t,
                    Err(_) => {
                        warn!("Invalid UTF-8 from {}", peer_addr);
                        continue;
                    }
                };

                // Validate frame size
                if text.len() > MAX_FRAME_SIZE {
                    let err = ServerMessage::Error {
                        message: format!("Message too large (max {} bytes)", MAX_FRAME_SIZE),
                        correlation_id: None,
                    };
                    if let Ok(json) = serde_json::to_string(&err) {
                        let _ = ws_send(&mut sender, json).await;
                    }
                    continue;
                }

                match serde_json::from_str::<ClientOperation>(text) {
                    Ok(op) => {
                        if let Err(e) = handle_operation(
                            &bus,
                            &connection_id,
                            &mut subscriptions,
                            &mut sender,
                            &forward_tx,
                            op,
                        ).await {
                            let error_msg = ServerMessage::Error {
                                message: format!("{}", e),
                                correlation_id: None,
                            };
                            if let Ok(json) = serde_json::to_string(&error_msg) {
                                let _ = ws_send(&mut sender, json).await;
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Invalid operation from {}: {}", peer_addr, e);
                        let error_msg = ServerMessage::Error {
                            message: format!("Invalid operation: {}", e),
                            correlation_id: None,
                        };
                        if let Ok(json) = serde_json::to_string(&error_msg) {
                            let _ = ws_send(&mut sender, json).await;
                        }
                    }
                }
            }

            // Forwarded subscription events to send to the client
            Some(json_str) = forward_rx.recv() => {
                if ws_send(&mut sender, json_str).await.is_err() {
                    break;
                }
            }
        }
    }

    // === Disconnect cleanup (Critical #4) ===
    // Explicitly unsubscribe all active subscriptions for this connection.
    let sub_ids: Vec<String> = subscriptions.keys().cloned().collect();
    for sub_id in &sub_ids {
        if let Err(e) = bus.unsubscribe(sub_id).await {
            debug!(
                "Failed to unsubscribe {} on disconnect from {}: {}",
                sub_id, peer_addr, e
            );
        }
    }
    if !sub_ids.is_empty() {
        debug!(
            "Cleaned up {} subscriptions for disconnected client {}",
            sub_ids.len(),
            peer_addr
        );
    }

    debug!("WebSocket client disconnected: {}", peer_addr);
    Ok(())
}

/// Helper to send a text message over WebSocket, mapping errors to our error type.
async fn ws_send(
    sender: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<TcpStream>,
        tokio_tungstenite::tungstenite::Message,
    >,
    text: String,
) -> crate::Result<()> {
    sender
        .send(tokio_tungstenite::tungstenite::Message::text(text))
        .await
        .map_err(|e| crate::EventBusError::Internal(format!("WebSocket send failed: {}", e)))
}

/// Validate and decode a base64 payload, enforcing size limits.
fn validate_and_decode_payload(payload: &str) -> crate::Result<Vec<u8>> {
    if payload.len() > MAX_PAYLOAD_SIZE {
        return Err(crate::EventBusError::PayloadTooLarge(format!(
            "Encoded payload {} bytes exceeds limit of {} bytes",
            payload.len(),
            MAX_PAYLOAD_SIZE
        )));
    }
    base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|e| crate::EventBusError::Internal(format!("Base64 decode failed: {}", e)))
}

async fn handle_operation(
    bus: &Arc<dyn EventBus>,
    connection_id: &str,
    subscriptions: &mut HashMap<String, ConnectionSubscription>,
    sender: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<TcpStream>,
        tokio_tungstenite::tungstenite::Message,
    >,
    forward_tx: &mpsc::Sender<String>,
    op: ClientOperation,
) -> crate::Result<()> {
    match op {
        ClientOperation::Publish {
            subject,
            payload,
            correlation_id: _,
        } => {
            let decoded = validate_and_decode_payload(&payload)?;
            bus.publish(&subject, &decoded).await?;
            debug!("Published to {} (base64 payload)", subject);
        }

        ClientOperation::Subscribe {
            pattern,
            opts,
            correlation_id,
        } => {
            // Enforce per-connection subscription limit (Critical #1)
            if subscriptions.len() >= MAX_SUBSCRIPTIONS_PER_CONNECTION {
                return Err(crate::EventBusError::Internal(format!(
                    "Subscription limit reached ({} max per connection)",
                    MAX_SUBSCRIPTIONS_PER_CONNECTION
                )));
            }

            let channel = ChannelConfig::WebSocket {
                connection_id: connection_id.to_string(),
            };

            let subscribe_opts = SubscribeOpts {
                priority: opts.priority,
                mode: SubscriptionMode::Async,
                channel,
                timeout: Duration::from_secs(5),
                filter: None,
            };

            let sub = bus.subscribe(&pattern, subscribe_opts).await?;
            let sub_id = sub.id.clone();

            // Spawn a task to forward events from this subscription's receiver
            // to the shared forward channel, so the main select! loop can send them.
            let forward_tx_clone = forward_tx.clone();
            let sub_id_for_task = sub_id.clone();
            let mut rx = sub.rx;
            tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    let payload_b64 =
                        base64::engine::general_purpose::STANDARD.encode(&event.payload);
                    let msg = ServerMessage::Event {
                        subject: event.subject.clone(),
                        payload: payload_b64,
                        reply_subject: event.reply_subject.clone(),
                        subscription_id: sub_id_for_task.clone(),
                    };
                    if let Ok(json) = serde_json::to_string(&msg) {
                        if forward_tx_clone.send(json).await.is_err() {
                            // Connection closed, stop forwarding
                            break;
                        }
                    }
                }
            });

            // Store subscription (rx is owned by the forwarder task above)
            subscriptions.insert(
                sub_id.clone(),
                ConnectionSubscription {
                    pattern: pattern.clone(),
                },
            );

            debug!("Subscribed to pattern: {} (id={})", pattern, sub_id);

            let response = ServerMessage::Subscribed {
                id: sub_id,
                correlation_id,
            };
            if let Ok(json) = serde_json::to_string(&response) {
                ws_send(sender, json).await?;
            }
        }

        ClientOperation::Unsubscribe {
            id,
            correlation_id: _,
        } => {
            if let Some(conn_sub) = subscriptions.remove(&id) {
                bus.unsubscribe(&id).await?;
                debug!(
                    "Unsubscribed from pattern: {} (id={})",
                    conn_sub.pattern, id
                );
            }
        }

        ClientOperation::Request {
            subject,
            payload,
            timeout_ms,
            correlation_id,
        } => {
            let decoded = validate_and_decode_payload(&payload)?;

            let timeout = Duration::from_millis(timeout_ms);
            match bus.request(&subject, &decoded, timeout).await {
                Ok(reply_payload) => {
                    let reply_b64 =
                        base64::engine::general_purpose::STANDARD.encode(&reply_payload);
                    let response = ServerMessage::ReplyResult {
                        payload: reply_b64,
                        correlation_id,
                    };
                    if let Ok(json) = serde_json::to_string(&response) {
                        ws_send(sender, json).await?;
                    }
                }
                Err(e) => {
                    let response = ServerMessage::Error {
                        message: format!("{}", e),
                        correlation_id,
                    };
                    if let Ok(json) = serde_json::to_string(&response) {
                        ws_send(sender, json).await?;
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_publish() {
        let json = json!({
            "op": "publish",
            "subject": "test.topic",
            "payload": "aGVsbG8="
        });

        let op: ClientOperation = serde_json::from_value(json).unwrap();
        match op {
            ClientOperation::Publish {
                subject, payload, ..
            } => {
                assert_eq!(subject, "test.topic");
                assert_eq!(payload, "aGVsbG8=");
            }
            _ => panic!("Wrong operation type"),
        }
    }

    #[test]
    fn test_parse_publish_with_correlation_id() {
        let json = json!({
            "op": "publish",
            "subject": "test.topic",
            "payload": "aGVsbG8=",
            "correlation_id": "req-123"
        });

        let op: ClientOperation = serde_json::from_value(json).unwrap();
        match op {
            ClientOperation::Publish { correlation_id, .. } => {
                assert_eq!(correlation_id, Some("req-123".to_string()));
            }
            _ => panic!("Wrong operation type"),
        }
    }

    #[test]
    fn test_parse_subscribe() {
        let json = json!({
            "op": "subscribe",
            "pattern": "test.*",
            "opts": {
                "priority": 10
            }
        });

        let op: ClientOperation = serde_json::from_value(json).unwrap();
        match op {
            ClientOperation::Subscribe { pattern, opts, .. } => {
                assert_eq!(pattern, "test.*");
                assert_eq!(opts.priority, 10);
            }
            _ => panic!("Wrong operation type"),
        }
    }

    #[test]
    fn test_server_message_event() {
        let msg = ServerMessage::Event {
            subject: "test.subject".to_string(),
            payload: "aGVsbG8=".to_string(),
            reply_subject: None,
            subscription_id: "sub-1".to_string(),
        };

        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["op"], "event");
        assert_eq!(json["subject"], "test.subject");
        assert_eq!(json["subscription_id"], "sub-1");
    }

    #[test]
    fn test_server_message_error_with_correlation() {
        let msg = ServerMessage::Error {
            message: "something failed".to_string(),
            correlation_id: Some("req-456".to_string()),
        };

        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["op"], "error");
        assert_eq!(json["correlation_id"], "req-456");
    }

    #[test]
    fn test_validate_payload_too_large() {
        let huge = "A".repeat(MAX_PAYLOAD_SIZE + 1);
        let result = validate_and_decode_payload(&huge);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("too large"));
    }

    #[test]
    fn test_validate_payload_ok() {
        let result = validate_and_decode_payload("aGVsbG8=");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), b"hello");
    }

    #[test]
    fn test_validate_payload_invalid_base64() {
        let result = validate_and_decode_payload("not-valid-base64!!!");
        assert!(result.is_err());
    }
}
