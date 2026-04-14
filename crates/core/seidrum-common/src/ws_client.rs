//! WebSocket client adapter for the seidrum-eventbus WS transport.
//!
//! `WsClient` speaks the JSON protocol defined in
//! `seidrum-eventbus/src/transport/ws.rs` and exposes an API shape
//! that mirrors [`super::bus_client::BusClient`]. In Phase 5 Step 2,
//! `BusClient` will be refactored to delegate to `WsClient` (for
//! plugin processes) or an in-process `EventBus` (for the kernel).
//!
//! **Architecture:**
//! ```text
//! WsClient
//! ├── writer_tx: mpsc::Sender<String>    ← methods push JSON frames here
//! ├── reader task (background)           ← demuxes incoming frames
//! │   ├── Event → subscribers[subscription_id].send(msg)
//! │   ├── Published/Subscribed/ReplyResult/Error → pending[correlation_id].send(reply)
//! │   └── connection closed → mark unhealthy
//! └── subscribers: Arc<DashMap<String, mpsc::Sender<WsMessage>>>
//! ```
//!
//! One WS connection per client. No auto-reconnect in v1; operations
//! fail immediately if the connection drops.

use anyhow::{Context, Result};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, info, warn};

/// A message received from the bus via a WebSocket subscription.
/// Field names mirror the bus Message type so consumers can switch
/// backends without source-level changes.
#[derive(Debug, Clone)]
pub struct WsMessage {
    /// The subject this message was published on.
    pub subject: String,
    /// Raw payload bytes (already decoded from the wire's base64).
    pub payload: bytes::Bytes,
    /// If this message is part of a request/reply exchange, the reply
    /// subject the handler should respond on. `None` for fire-and-forget
    /// publishes.
    pub reply: Option<String>,
}

/// A subscription handle that yields [`WsMessage`]s. Drop it to stop
/// receiving events (the WsClient will eventually unsubscribe from the
/// server, though the current implementation does not auto-unsubscribe
/// on drop — call [`WsClient::unsubscribe`] explicitly).
pub struct WsSubscription {
    /// Channel for receiving messages. `pub(crate)` so `BusClient` can
    /// construct a `WsSubscription` from the NATS bridge task.
    pub(crate) rx: mpsc::Receiver<WsMessage>,
    /// The bus-assigned subscription id (used for unsubscribe).
    pub id: String,
}

impl WsSubscription {
    /// Receive the next message. Returns `None` when the subscription
    /// is closed (server unsubscribed, or the WsClient dropped).
    pub async fn next(&mut self) -> Option<WsMessage> {
        self.rx.recv().await
    }
}

/// Subject type alias — just a `String` for the WS backend. Matches
/// the bus Subject type shape.
pub type WsSubject = String;

// === Wire protocol types (must match seidrum-eventbus ws.rs) ===

#[derive(Debug, Serialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum ClientOp {
    #[serde(rename = "publish")]
    Publish {
        subject: String,
        payload: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        correlation_id: Option<String>,
    },
    #[serde(rename = "subscribe")]
    Subscribe {
        pattern: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        correlation_id: Option<String>,
    },
    #[serde(rename = "unsubscribe")]
    Unsubscribe { id: String },
    #[serde(rename = "request")]
    Request {
        subject: String,
        payload: String,
        timeout_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        correlation_id: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum ServerMsg {
    #[serde(rename = "published")]
    Published {
        #[allow(dead_code)]
        seq: u64,
        correlation_id: Option<String>,
    },
    #[serde(rename = "subscribed")]
    Subscribed {
        id: String,
        correlation_id: Option<String>,
    },
    #[serde(rename = "event")]
    Event {
        subject: String,
        payload: String,
        reply_subject: Option<String>,
        subscription_id: String,
    },
    #[serde(rename = "reply_result")]
    ReplyResult {
        payload: String,
        correlation_id: Option<String>,
    },
    #[serde(rename = "error")]
    Error {
        message: String,
        correlation_id: Option<String>,
    },
    // --- Variants the client doesn't use but the server may send ---
    // Listed so `serde_json::from_str` doesn't fail on them. The reader
    // silently ignores these. If any carry a `correlation_id`, we don't
    // route them — the pending entry will eventually be cleaned up by
    // the caller's timeout or drop.
    #[serde(rename = "channel_registered")]
    ChannelRegistered {
        #[allow(dead_code)]
        channel_type: String,
        #[allow(dead_code)]
        correlation_id: Option<String>,
    },
    #[serde(rename = "interceptor_registered")]
    InterceptorRegistered {
        #[allow(dead_code)]
        id: String,
        #[allow(dead_code)]
        correlation_id: Option<String>,
    },
    #[serde(rename = "deliver")]
    Deliver {
        #[allow(dead_code)]
        request_id: String,
        #[allow(dead_code)]
        channel_type: String,
        #[allow(dead_code)]
        subject: String,
        #[allow(dead_code)]
        payload: String,
    },
    #[serde(rename = "intercept")]
    Intercept {
        #[allow(dead_code)]
        request_id: String,
        #[allow(dead_code)]
        subject: String,
        #[allow(dead_code)]
        payload: String,
    },
}

/// Internal reply type for pending operations.
enum PendingReply {
    Published,
    Subscribed(String),   // subscription id
    ReplyResult(Vec<u8>), // decoded payload
    Error(String),        // error message
}

/// WebSocket client for the seidrum-eventbus transport server.
///
/// Created via [`WsClient::connect`]. All methods are `&self` — the
/// client is `Clone`-safe (internally Arc'd).
/// Default request timeout in milliseconds. Used by `request_bytes`
/// and `request` unless overridden via `with_request_timeout`.
const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 5000;

#[derive(Clone)]
pub struct WsClient {
    writer_tx: mpsc::Sender<String>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<PendingReply>>>>,
    subscribers: Arc<Mutex<HashMap<String, mpsc::Sender<WsMessage>>>>,
    connected: Arc<AtomicBool>,
    pub source: String,
    /// Per-request timeout in milliseconds. Sent to the server in the
    /// `Request` operation. Configurable via [`Self::with_request_timeout`].
    request_timeout_ms: u64,
}

impl WsClient {
    /// Connect to the eventbus WS server at `url` (e.g. `ws://127.0.0.1:9000`).
    /// `source` is the plugin/service identifier stamped onto envelopes.
    pub async fn connect(url: &str, source: &str) -> Result<Self> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(url)
            .await
            .with_context(|| format!("failed to connect to eventbus WS at {url}"))?;
        info!(url, source, "connected to eventbus WS");

        let (ws_writer, ws_reader) = ws_stream.split();
        let (writer_tx, writer_rx) = mpsc::channel::<String>(256);
        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<PendingReply>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let subscribers: Arc<Mutex<HashMap<String, mpsc::Sender<WsMessage>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let connected = Arc::new(AtomicBool::new(true));

        // Spawn writer task: drains writer_rx and sends frames.
        let connected_w = Arc::clone(&connected);
        tokio::spawn(async move {
            let mut ws_writer = ws_writer;
            let mut writer_rx = writer_rx;
            while let Some(frame) = writer_rx.recv().await {
                if ws_writer
                    .send(tokio_tungstenite::tungstenite::Message::text(frame))
                    .await
                    .is_err()
                {
                    connected_w.store(false, Ordering::SeqCst);
                    break;
                }
            }
        });

        // Spawn reader task: demuxes incoming frames.
        let pending_r = Arc::clone(&pending);
        let subscribers_r = Arc::clone(&subscribers);
        let connected_r = Arc::clone(&connected);
        tokio::spawn(async move {
            let mut ws_reader = ws_reader;
            while let Some(frame_result) = ws_reader.next().await {
                let frame = match frame_result {
                    Ok(f) => f,
                    Err(e) => {
                        warn!(error = %e, "WsClient read error");
                        break;
                    }
                };

                if frame.is_close() {
                    break;
                }
                if !frame.is_text() {
                    continue;
                }

                let text = match frame.to_text() {
                    Ok(t) => t,
                    Err(_) => continue,
                };

                let msg: ServerMsg = match serde_json::from_str(text) {
                    Ok(m) => m,
                    Err(e) => {
                        debug!(error = %e, "WsClient received unparseable frame");
                        continue;
                    }
                };

                match msg {
                    ServerMsg::Published { correlation_id, .. } => {
                        if let Some(cid) = correlation_id {
                            let mut p = pending_r.lock().await;
                            if let Some(tx) = p.remove(&cid) {
                                let _ = tx.send(PendingReply::Published);
                            }
                        }
                    }
                    ServerMsg::Subscribed { id, correlation_id } => {
                        if let Some(cid) = correlation_id {
                            let mut p = pending_r.lock().await;
                            if let Some(tx) = p.remove(&cid) {
                                let _ = tx.send(PendingReply::Subscribed(id));
                            }
                        }
                    }
                    ServerMsg::ReplyResult {
                        payload,
                        correlation_id,
                    } => {
                        if let Some(cid) = correlation_id {
                            let decoded = base64::engine::general_purpose::STANDARD
                                .decode(&payload)
                                .unwrap_or_default();
                            let mut p = pending_r.lock().await;
                            if let Some(tx) = p.remove(&cid) {
                                let _ = tx.send(PendingReply::ReplyResult(decoded));
                            }
                        }
                    }
                    ServerMsg::Error {
                        message,
                        correlation_id,
                    } => {
                        if let Some(cid) = correlation_id {
                            let mut p = pending_r.lock().await;
                            if let Some(tx) = p.remove(&cid) {
                                let _ = tx.send(PendingReply::Error(message));
                            }
                        } else {
                            warn!(message = %message, "WsClient received error without correlation_id");
                        }
                    }
                    ServerMsg::Event {
                        subject,
                        payload,
                        reply_subject,
                        subscription_id,
                    } => {
                        let decoded = base64::engine::general_purpose::STANDARD
                            .decode(&payload)
                            .unwrap_or_default();
                        let msg = WsMessage {
                            subject,
                            payload: bytes::Bytes::from(decoded),
                            reply: reply_subject,
                        };
                        let subs = subscribers_r.lock().await;
                        if let Some(tx) = subs.get(&subscription_id) {
                            let _ = tx.send(msg).await;
                        }
                    }
                    // Variants the client doesn't actively use but the
                    // server may send. Silently ignored so the reader
                    // doesn't crash on unexpected frames.
                    ServerMsg::ChannelRegistered { .. }
                    | ServerMsg::InterceptorRegistered { .. }
                    | ServerMsg::Deliver { .. }
                    | ServerMsg::Intercept { .. } => {}
                }
            }
            connected_r.store(false, Ordering::SeqCst);
            debug!("WsClient reader task exited");
        });

        Ok(Self {
            writer_tx,
            pending,
            subscribers,
            connected,
            source: source.to_string(),
            request_timeout_ms: DEFAULT_REQUEST_TIMEOUT_MS,
        })
    }

    /// Override the per-request timeout (default 5000ms). This value
    /// is sent to the server in every `Request` operation; the server
    /// enforces it server-side.
    pub fn with_request_timeout(mut self, timeout_ms: u64) -> Self {
        self.request_timeout_ms = timeout_ms;
        self
    }

    fn next_correlation_id() -> String {
        ulid::Ulid::new().to_string()
    }

    async fn send_and_wait(&self, op: &ClientOp, cid: &str) -> Result<PendingReply> {
        let (tx, rx) = oneshot::channel();
        {
            let mut p = self.pending.lock().await;
            p.insert(cid.to_string(), tx);
        }

        let frame = serde_json::to_string(op).context("failed to serialize WS operation")?;
        if let Err(e) = self.writer_tx.send(frame).await {
            // Clean up the pending entry so it doesn't leak.
            self.pending.lock().await.remove(cid);
            return Err(anyhow::anyhow!("WsClient connection closed: {}", e));
        }

        let reply = rx
            .await
            .map_err(|_| anyhow::anyhow!("WsClient pending reply dropped (connection lost)"))?;

        if let PendingReply::Error(msg) = &reply {
            return Err(anyhow::anyhow!("bus error: {}", msg));
        }

        Ok(reply)
    }

    /// Publish a raw byte payload to a subject.
    pub async fn publish_bytes(
        &self,
        subject: impl AsRef<str>,
        payload: impl AsRef<[u8]>,
    ) -> Result<()> {
        let cid = Self::next_correlation_id();
        let payload_b64 = base64::engine::general_purpose::STANDARD.encode(payload.as_ref());
        let op = ClientOp::Publish {
            subject: subject.as_ref().to_string(),
            payload: payload_b64,
            correlation_id: Some(cid.clone()),
        };
        self.send_and_wait(&op, &cid).await?;
        Ok(())
    }

    /// Publish a serializable payload to a subject.
    pub async fn publish<T: Serialize>(&self, subject: impl AsRef<str>, payload: &T) -> Result<()> {
        let bytes = serde_json::to_vec(payload).context("failed to serialize payload")?;
        self.publish_bytes(subject, bytes).await
    }

    /// Publish a payload wrapped in an [`crate::events::EventEnvelope`].
    pub async fn publish_envelope<T: Serialize>(
        &self,
        subject: &str,
        correlation_id: Option<String>,
        scope: Option<String>,
        payload: &T,
    ) -> Result<crate::events::EventEnvelope> {
        let envelope = crate::events::EventEnvelope::new(
            subject,
            &self.source,
            correlation_id,
            scope,
            payload,
        )
        .context("failed to build EventEnvelope")?;
        self.publish(subject, &envelope).await?;
        Ok(envelope)
    }

    /// Subscribe to a subject pattern.
    pub async fn subscribe(&self, subject: impl AsRef<str>) -> Result<WsSubscription> {
        let cid = Self::next_correlation_id();
        let op = ClientOp::Subscribe {
            pattern: subject.as_ref().to_string(),
            correlation_id: Some(cid.clone()),
        };
        let reply = self.send_and_wait(&op, &cid).await?;
        let subscription_id = match reply {
            PendingReply::Subscribed(id) => id,
            _ => return Err(anyhow::anyhow!("unexpected reply to subscribe")),
        };

        let (tx, rx) = mpsc::channel::<WsMessage>(256);
        {
            let mut subs = self.subscribers.lock().await;
            subs.insert(subscription_id.clone(), tx);
        }

        Ok(WsSubscription {
            rx,
            id: subscription_id,
        })
    }

    /// Unsubscribe from a subscription by id.
    pub async fn unsubscribe(&self, id: &str) -> Result<()> {
        {
            let mut subs = self.subscribers.lock().await;
            subs.remove(id);
        }
        let op = ClientOp::Unsubscribe { id: id.to_string() };
        let frame = serde_json::to_string(&op)?;
        let _ = self.writer_tx.send(frame).await;
        Ok(())
    }

    /// Send a raw byte request and return the raw byte response.
    pub async fn request_bytes(
        &self,
        subject: impl AsRef<str>,
        payload: impl AsRef<[u8]>,
    ) -> Result<Vec<u8>> {
        let cid = Self::next_correlation_id();
        let payload_b64 = base64::engine::general_purpose::STANDARD.encode(payload.as_ref());
        let op = ClientOp::Request {
            subject: subject.as_ref().to_string(),
            payload: payload_b64,
            timeout_ms: self.request_timeout_ms,
            correlation_id: Some(cid.clone()),
        };
        let reply = self.send_and_wait(&op, &cid).await?;
        match reply {
            PendingReply::ReplyResult(bytes) => Ok(bytes),
            _ => Err(anyhow::anyhow!("unexpected reply to request")),
        }
    }

    /// Send a typed request and deserialize the response.
    pub async fn request<T: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        subject: impl AsRef<str>,
        payload: &T,
    ) -> Result<R> {
        let bytes = serde_json::to_vec(payload).context("failed to serialize request")?;
        let response = self.request_bytes(subject, bytes).await?;
        let result: R =
            serde_json::from_slice(&response).context("failed to deserialize response")?;
        Ok(result)
    }

    /// Reply to a captured reply subject.
    pub async fn reply_to(&self, reply_subject: &str, payload: impl AsRef<[u8]>) -> Result<()> {
        self.publish_bytes(reply_subject, payload).await
    }

    /// Returns `true` if the underlying WebSocket connection is alive.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_op_publish_serializes() {
        let op = ClientOp::Publish {
            subject: "test.subject".to_string(),
            payload: "aGVsbG8=".to_string(),
            correlation_id: Some("cid-1".to_string()),
        };
        let json = serde_json::to_value(&op).unwrap();
        assert_eq!(json["op"], "publish");
        assert_eq!(json["subject"], "test.subject");
        assert_eq!(json["payload"], "aGVsbG8=");
        assert_eq!(json["correlation_id"], "cid-1");
    }

    #[test]
    fn test_client_op_subscribe_serializes() {
        let op = ClientOp::Subscribe {
            pattern: "events.>".to_string(),
            correlation_id: Some("cid-2".to_string()),
        };
        let json = serde_json::to_value(&op).unwrap();
        assert_eq!(json["op"], "subscribe");
        assert_eq!(json["pattern"], "events.>");
    }

    #[test]
    fn test_client_op_request_serializes() {
        let op = ClientOp::Request {
            subject: "brain.query".to_string(),
            payload: "cGF5bG9hZA==".to_string(),
            timeout_ms: 3000,
            correlation_id: Some("cid-3".to_string()),
        };
        let json = serde_json::to_value(&op).unwrap();
        assert_eq!(json["op"], "request");
        assert_eq!(json["timeout_ms"], 3000);
    }

    #[test]
    fn test_server_msg_published_deserializes() {
        let json = r#"{"op":"published","seq":42,"correlation_id":"cid-1"}"#;
        let msg: ServerMsg = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, ServerMsg::Published { .. }));
    }

    #[test]
    fn test_server_msg_subscribed_deserializes() {
        let json = r#"{"op":"subscribed","id":"sub-abc","correlation_id":"cid-2"}"#;
        let msg: ServerMsg = serde_json::from_str(json).unwrap();
        match msg {
            ServerMsg::Subscribed { id, correlation_id } => {
                assert_eq!(id, "sub-abc");
                assert_eq!(correlation_id, Some("cid-2".to_string()));
            }
            _ => panic!("expected Subscribed"),
        }
    }

    #[test]
    fn test_server_msg_event_deserializes() {
        let json = r#"{"op":"event","subject":"test.foo","payload":"aGVsbG8=","reply_subject":null,"subscription_id":"sub-1"}"#;
        let msg: ServerMsg = serde_json::from_str(json).unwrap();
        match msg {
            ServerMsg::Event {
                subject,
                payload,
                reply_subject,
                subscription_id,
            } => {
                assert_eq!(subject, "test.foo");
                assert_eq!(payload, "aGVsbG8=");
                assert!(reply_subject.is_none());
                assert_eq!(subscription_id, "sub-1");
            }
            _ => panic!("expected Event"),
        }
    }

    #[test]
    fn test_server_msg_error_deserializes() {
        let json = r#"{"op":"error","message":"something broke","correlation_id":"cid-4"}"#;
        let msg: ServerMsg = serde_json::from_str(json).unwrap();
        match msg {
            ServerMsg::Error {
                message,
                correlation_id,
            } => {
                assert_eq!(message, "something broke");
                assert_eq!(correlation_id, Some("cid-4".to_string()));
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn test_server_msg_reply_result_deserializes() {
        let json = r#"{"op":"reply_result","payload":"cmVzdWx0","correlation_id":"cid-5"}"#;
        let msg: ServerMsg = serde_json::from_str(json).unwrap();
        match msg {
            ServerMsg::ReplyResult {
                payload,
                correlation_id,
            } => {
                assert_eq!(payload, "cmVzdWx0");
                assert_eq!(correlation_id, Some("cid-5".to_string()));
            }
            _ => panic!("expected ReplyResult"),
        }
    }
}
