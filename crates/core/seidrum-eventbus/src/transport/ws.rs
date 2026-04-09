//! WebSocket transport server.
//!
//! Provides a WebSocket server that allows remote clients to connect
//! and interact with the event bus using a JSON protocol.

use crate::bus::{EventBus, SubscribeOpts};
use crate::delivery::ChannelConfig;
use crate::dispatch::SubscriptionMode;
use crate::request_reply::DispatchedEvent;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use ulid::Ulid;

/// WebSocket protocol operations.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op")]
#[serde(rename_all = "lowercase")]
pub enum ClientOperation {
    /// Publish an event
    #[serde(rename = "publish")]
    Publish { subject: String, payload: String }, // base64
    /// Subscribe to a pattern
    #[serde(rename = "subscribe")]
    Subscribe {
        pattern: String,
        #[serde(default)]
        opts: SubscribeOptions,
    },
    /// Unsubscribe from a subscription
    #[serde(rename = "unsubscribe")]
    Unsubscribe { id: String },
    /// Send a request and wait for reply
    #[serde(rename = "request")]
    Request {
        subject: String,
        payload: String, // base64
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    /// Reply to a request
    #[serde(rename = "reply")]
    Reply {
        reply_subject: String,
        payload: String, // base64
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

/// Outbound message to client.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op")]
#[serde(rename_all = "lowercase")]
pub enum ServerMessage {
    /// Event delivered to subscriber
    #[serde(rename = "event")]
    Event {
        subject: String,
        payload: String, // base64
        reply_subject: Option<String>,
    },
    /// Reply to a request
    #[serde(rename = "reply_result")]
    ReplyResult { payload: String }, // base64
    /// Error message
    #[serde(rename = "error")]
    Error { message: String },
}

/// WebSocket server for remote event bus clients.
pub struct WebSocketServer {
    bus: Arc<dyn EventBus>,
}

impl WebSocketServer {
    /// Create a new WebSocket server.
    pub fn new(bus: Arc<dyn EventBus>) -> Self {
        Self { bus }
    }

    /// Start the WebSocket server on the given address.
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
            tokio::spawn(async move {
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
    let mut subscriptions: HashMap<String, (String, mpsc::Receiver<DispatchedEvent>)> =
        HashMap::new();

    while let Some(msg_result) = receiver.next().await {
        let msg = match msg_result {
            Ok(msg) => msg,
            Err(e) => {
                debug!("WebSocket error from {}: {}", peer_addr, e);
                break;
            }
        };

        if !msg.is_text() {
            warn!("Non-text message from {}, ignoring", peer_addr);
            continue;
        }

        let text = match msg.to_text() {
            Ok(t) => t,
            Err(_) => {
                warn!("Invalid UTF-8 from {}", peer_addr);
                continue;
            }
        };

        match serde_json::from_str::<ClientOperation>(text) {
            Ok(op) => {
                if let Err(e) =
                    handle_operation(&bus, &connection_id, &mut subscriptions, &mut sender, op)
                        .await
                {
                    let error_msg = ServerMessage::Error {
                        message: format!("{}", e),
                    };
                    if let Ok(json) = serde_json::to_string(&error_msg) {
                        let _ = sender
                            .send(tokio_tungstenite::tungstenite::Message::text(json))
                            .await;
                    }
                }
            }
            Err(e) => {
                warn!("Invalid operation from {}: {}", peer_addr, e);
                let error_msg = ServerMessage::Error {
                    message: format!("Invalid operation: {}", e),
                };
                if let Ok(json) = serde_json::to_string(&error_msg) {
                    let _ = sender
                        .send(tokio_tungstenite::tungstenite::Message::text(json))
                        .await;
                }
            }
        }
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

async fn handle_operation(
    bus: &Arc<dyn EventBus>,
    connection_id: &str,
    subscriptions: &mut HashMap<String, (String, mpsc::Receiver<DispatchedEvent>)>,
    sender: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<TcpStream>,
        tokio_tungstenite::tungstenite::Message,
    >,
    op: ClientOperation,
) -> crate::Result<()> {
    match op {
        ClientOperation::Publish { subject, payload } => {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(&payload)
                .map_err(|e| {
                    crate::EventBusError::Internal(format!("Base64 decode failed: {}", e))
                })?;

            bus.publish(&subject, &decoded).await?;
            debug!("Published to {} (base64 payload)", subject);
        }

        ClientOperation::Subscribe { pattern, opts } => {
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

            subscriptions.insert(sub_id.clone(), (pattern.clone(), sub.rx));

            debug!("Subscribed to pattern: {} (id={})", pattern, sub_id);

            let response = json!({
                "op": "subscribed",
                "id": sub_id,
            });
            ws_send(sender, response.to_string()).await?;
        }

        ClientOperation::Unsubscribe { id } => {
            if let Some((pattern, _rx)) = subscriptions.remove(&id) {
                bus.unsubscribe(&id).await?;
                debug!("Unsubscribed from pattern: {} (id={})", pattern, id);
            }
        }

        ClientOperation::Request {
            subject,
            payload,
            timeout_ms,
        } => {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(&payload)
                .map_err(|e| {
                    crate::EventBusError::Internal(format!("Base64 decode failed: {}", e))
                })?;

            let timeout = Duration::from_millis(timeout_ms);
            match bus.request(&subject, &decoded, timeout).await {
                Ok(reply_payload) => {
                    let reply_b64 =
                        base64::engine::general_purpose::STANDARD.encode(&reply_payload);
                    let response = ServerMessage::ReplyResult { payload: reply_b64 };
                    if let Ok(json) = serde_json::to_string(&response) {
                        ws_send(sender, json).await?;
                    }
                }
                Err(e) => {
                    let response = ServerMessage::Error {
                        message: format!("{}", e),
                    };
                    if let Ok(json) = serde_json::to_string(&response) {
                        ws_send(sender, json).await?;
                    }
                }
            }
        }

        ClientOperation::Reply {
            reply_subject,
            payload,
        } => {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(&payload)
                .map_err(|e| {
                    crate::EventBusError::Internal(format!("Base64 decode failed: {}", e))
                })?;

            // Note: _reply.* subjects are reserved and rejected by bus.publish().
            // Remote clients should use the serve()/Replier pattern for
            // request/reply. This publish call will fail for _reply.* subjects.
            // A dedicated reply endpoint using engine.publish_event() would be
            // needed for full remote serve() support (Phase 5 integration).
            bus.publish(&reply_subject, &decoded).await?;
            debug!("Replied to {}", reply_subject);
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
            ClientOperation::Publish { subject, payload } => {
                assert_eq!(subject, "test.topic");
                assert_eq!(payload, "aGVsbG8=");
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
            ClientOperation::Subscribe { pattern, opts } => {
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
        };

        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["op"], "event");
        assert_eq!(json["subject"], "test.subject");
    }
}
