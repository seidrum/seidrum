//! WebSocket client adapter for the seidrum-eventbus WS transport.
//!
//! `WsClient` speaks the JSON protocol defined in
//! `seidrum-eventbus/src/transport/ws.rs` and exposes an API shape
//! that mirrors [`super::bus_client::BusClient`].
//!
//! The client owns a small connection manager task. Subscriptions are tracked
//! by stable client-side ids so a reconnect can re-send subscribe operations,
//! learn the new transient server-assigned ids, and keep existing
//! [`WsSubscription`] handles alive.

use anyhow::{Context, Result};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
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
    /// Stable client-side subscription id. Server-assigned ids are transient
    /// and may change after reconnect.
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

#[derive(Debug)]
struct SubscriptionState {
    stable_id: String,
    pattern: String,
    server_id: Option<String>,
    sender: mpsc::Sender<WsMessage>,
}

#[derive(Default, Debug)]
struct SubscriberRegistry {
    by_stable_id: HashMap<String, SubscriptionState>,
    server_to_stable: HashMap<String, String>,
}

impl SubscriberRegistry {
    fn insert(&mut self, stable_id: String, pattern: String, sender: mpsc::Sender<WsMessage>) {
        self.by_stable_id.insert(
            stable_id.clone(),
            SubscriptionState {
                stable_id,
                pattern,
                server_id: None,
                sender,
            },
        );
    }

    fn bind_server_id(&mut self, stable_id: &str, server_id: String) -> bool {
        let Some(state) = self.by_stable_id.get_mut(stable_id) else {
            return false;
        };
        if let Some(old_server_id) = state.server_id.replace(server_id.clone()) {
            self.server_to_stable.remove(&old_server_id);
        }
        self.server_to_stable
            .insert(server_id, state.stable_id.clone());
        true
    }

    fn remove(&mut self, stable_id: &str) -> Option<SubscriptionState> {
        let state = self.by_stable_id.remove(stable_id)?;
        if let Some(server_id) = &state.server_id {
            self.server_to_stable.remove(server_id);
        }
        Some(state)
    }

    fn get_sender_by_server_id(&self, server_id: &str) -> Option<mpsc::Sender<WsMessage>> {
        let stable_id = self.server_to_stable.get(server_id)?;
        self.by_stable_id
            .get(stable_id)
            .map(|state| state.sender.clone())
    }

    fn active_patterns(&self) -> Vec<(String, String)> {
        self.by_stable_id
            .values()
            .map(|state| (state.stable_id.clone(), state.pattern.clone()))
            .collect()
    }

    fn clear_server_ids(&mut self) {
        self.server_to_stable.clear();
        for state in self.by_stable_id.values_mut() {
            state.server_id = None;
        }
    }
}

enum ManagerCommand {
    Send(String),
}

/// Default request timeout in milliseconds. Used by `request_bytes`
/// and `request` unless overridden via `with_request_timeout`.
const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 5000;
const DEFAULT_RECONNECT_INITIAL_BACKOFF_MS: u64 = 100;
const DEFAULT_RECONNECT_MAX_BACKOFF_MS: u64 = 30_000;
const RESUBSCRIBE_CID_PREFIX: &str = "__seidrum_resubscribe__:";

/// WebSocket client for the seidrum-eventbus transport server.
///
/// Created via [`WsClient::connect`]. All methods are `&self` — the
/// client is `Clone`-safe (internally Arc'd).
#[derive(Clone)]
pub struct WsClient {
    manager_tx: mpsc::Sender<ManagerCommand>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<PendingReply>>>>,
    subscribers: Arc<Mutex<SubscriberRegistry>>,
    connected: Arc<AtomicBool>,
    pub source: String,
    /// Per-request timeout in milliseconds. Sent to the server in the
    /// `Request` operation. Configurable via [`Self::with_request_timeout`].
    request_timeout_ms: u64,
    reconnect_initial_backoff: Duration,
    reconnect_max_backoff: Duration,
    max_reconnect_attempts: Option<usize>,
}

impl WsClient {
    /// Connect to the eventbus WS server at `url` (e.g. `ws://127.0.0.1:9000`).
    /// `source` is the plugin/service identifier stamped onto envelopes.
    pub async fn connect(url: &str, source: &str) -> Result<Self> {
        connect_with_options(
            url,
            source,
            Duration::from_millis(DEFAULT_RECONNECT_INITIAL_BACKOFF_MS),
            Duration::from_millis(DEFAULT_RECONNECT_MAX_BACKOFF_MS),
            None,
        )
        .await
    }

    /// Override the per-request timeout (default 5000ms). This value
    /// is sent to the server in every `Request` operation; the server
    /// enforces it server-side.
    pub fn with_request_timeout(mut self, timeout_ms: u64) -> Self {
        self.request_timeout_ms = timeout_ms;
        self
    }

    /// Override the reconnect backoff bounds used by clients created through
    /// [`Self::connect_with_reconnect`]. This builder is retained on cloned
    /// clients for observability but cannot reconfigure an already spawned
    /// manager; use [`Self::connect_with_reconnect`] when constructing a new
    /// client.
    pub fn with_reconnect_backoff(mut self, initial: Duration, max: Duration) -> Self {
        self.reconnect_initial_backoff = initial;
        self.reconnect_max_backoff = max.max(initial);
        self
    }

    /// Override the maximum reconnect attempts used by clients created through
    /// [`Self::connect_with_reconnect`]. `None` means retry forever.
    pub fn with_max_reconnect_attempts(mut self, max: Option<usize>) -> Self {
        self.max_reconnect_attempts = max;
        self
    }

    /// Connect with explicit reconnect configuration. This is the effective
    /// constructor for non-default reconnect settings.
    pub async fn connect_with_reconnect(
        url: &str,
        source: &str,
        initial_backoff: Duration,
        max_backoff: Duration,
        max_attempts: Option<usize>,
    ) -> Result<Self> {
        connect_with_options(url, source, initial_backoff, max_backoff, max_attempts).await
    }

    fn next_correlation_id() -> String {
        ulid::Ulid::new().to_string()
    }

    async fn wait_until_connected(&self, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.is_connected() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow::anyhow!(
                    "failed to connect or timed out connecting to eventbus WS"
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn send_and_wait(&self, op: &ClientOp, cid: &str) -> Result<PendingReply> {
        if !self.is_connected() {
            return Err(anyhow::anyhow!("WsClient is reconnecting"));
        }

        let (tx, rx) = oneshot::channel();
        {
            let mut p = self.pending.lock().await;
            p.insert(cid.to_string(), tx);
        }

        let frame = serde_json::to_string(op).context("failed to serialize WS operation")?;
        if let Err(e) = self.manager_tx.send(ManagerCommand::Send(frame)).await {
            // Clean up the pending entry so it doesn't leak.
            self.pending.lock().await.remove(cid);
            return Err(anyhow::anyhow!("WsClient connection manager closed: {e}"));
        }

        let reply = rx
            .await
            .map_err(|_| anyhow::anyhow!("WsClient pending reply dropped (connection lost)"))?;

        if let PendingReply::Error(msg) = &reply {
            return Err(anyhow::anyhow!("bus error: {msg}"));
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
        let stable_id = Self::next_correlation_id();
        let cid = Self::next_correlation_id();
        let pattern = subject.as_ref().to_string();
        let op = ClientOp::Subscribe {
            pattern: pattern.clone(),
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
            subs.insert(stable_id.clone(), pattern, tx);
            subs.bind_server_id(&stable_id, subscription_id);
        }

        Ok(WsSubscription { rx, id: stable_id })
    }

    /// Unsubscribe from a subscription by stable client-side id.
    pub async fn unsubscribe(&self, id: &str) -> Result<()> {
        let server_id = {
            let mut subs = self.subscribers.lock().await;
            subs.remove(id).and_then(|state| state.server_id)
        };
        if let Some(server_id) = server_id {
            let op = ClientOp::Unsubscribe { id: server_id };
            let frame = serde_json::to_string(&op)?;
            let _ = self.manager_tx.send(ManagerCommand::Send(frame)).await;
        }
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

async fn connect_with_options(
    url: &str,
    source: &str,
    initial_backoff: Duration,
    max_backoff: Duration,
    max_attempts: Option<usize>,
) -> Result<WsClient> {
    let pending: Arc<Mutex<HashMap<String, oneshot::Sender<PendingReply>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let subscribers = Arc::new(Mutex::new(SubscriberRegistry::default()));
    let connected = Arc::new(AtomicBool::new(false));
    let (manager_tx, manager_rx) = mpsc::channel::<ManagerCommand>(256);
    let reconnect_max_backoff = max_backoff.max(initial_backoff);
    let client = WsClient {
        manager_tx,
        pending: Arc::clone(&pending),
        subscribers: Arc::clone(&subscribers),
        connected: Arc::clone(&connected),
        source: source.to_string(),
        request_timeout_ms: DEFAULT_REQUEST_TIMEOUT_MS,
        reconnect_initial_backoff: initial_backoff,
        reconnect_max_backoff,
        max_reconnect_attempts: max_attempts,
    };
    tokio::spawn(connection_manager(ConnectionManagerState {
        url: url.to_string(),
        source: source.to_string(),
        manager_rx,
        pending,
        subscribers,
        connected,
        initial_backoff,
        max_backoff: reconnect_max_backoff,
        max_attempts,
    }));
    client.wait_until_connected(Duration::from_secs(5)).await?;
    Ok(client)
}

struct ConnectionManagerState {
    url: String,
    source: String,
    manager_rx: mpsc::Receiver<ManagerCommand>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<PendingReply>>>>,
    subscribers: Arc<Mutex<SubscriberRegistry>>,
    connected: Arc<AtomicBool>,
    initial_backoff: Duration,
    max_backoff: Duration,
    max_attempts: Option<usize>,
}

async fn connection_manager(mut state: ConnectionManagerState) {
    let mut backoff = state.initial_backoff;
    let mut failed_attempts = 0usize;

    loop {
        match tokio_tungstenite::connect_async(&state.url).await {
            Ok((ws_stream, _)) => {
                info!(url = %state.url, source = %state.source, "connected to eventbus WS");
                failed_attempts = 0;
                backoff = state.initial_backoff;
                state.connected.store(true, Ordering::SeqCst);

                if let Err(e) = resubscribe_active(
                    &mut state.manager_rx,
                    &state.pending,
                    &state.subscribers,
                    ws_stream,
                )
                .await
                {
                    warn!(error = %e, "WsClient connection dropped");
                }

                state.connected.store(false, Ordering::SeqCst);
                fail_all_pending(&state.pending).await;
                state.subscribers.lock().await.clear_server_ids();
                debug!("WsClient connection cycle exited");
            }
            Err(e) => {
                state.connected.store(false, Ordering::SeqCst);
                failed_attempts += 1;
                warn!(url = %state.url, source = %state.source, attempt = failed_attempts, error = %e, "failed to connect to eventbus WS");
                if state.max_attempts.is_some_and(|max| failed_attempts >= max) {
                    warn!(url = %state.url, source = %state.source, "WsClient reached max reconnect attempts");
                    fail_all_pending(&state.pending).await;
                    return;
                }
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {},
                    maybe_cmd = state.manager_rx.recv() => {
                        if maybe_cmd.is_none() {
                            return;
                        }
                        // Drop commands submitted while disconnected. Public methods
                        // reject new operations when `connected == false`; this branch
                        // handles the race where the connection closed after enqueue.
                    }
                }
                backoff = (backoff * 2).min(state.max_backoff);
            }
        }
    }
}

async fn resubscribe_active<S>(
    manager_rx: &mut mpsc::Receiver<ManagerCommand>,
    pending: &Arc<Mutex<HashMap<String, oneshot::Sender<PendingReply>>>>,
    subscribers: &Arc<Mutex<SubscriberRegistry>>,
    ws_stream: tokio_tungstenite::WebSocketStream<S>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut ws_writer, mut ws_reader) = ws_stream.split();

    let active = subscribers.lock().await.active_patterns();
    for (stable_id, pattern) in active {
        let cid = format!(
            "{RESUBSCRIBE_CID_PREFIX}{stable_id}:{}",
            WsClient::next_correlation_id()
        );
        let op = ClientOp::Subscribe {
            pattern,
            correlation_id: Some(cid),
        };
        let frame = serde_json::to_string(&op).context("failed to serialize resubscribe op")?;
        ws_writer
            .send(tokio_tungstenite::tungstenite::Message::text(frame))
            .await
            .context("failed to send resubscribe op")?;
    }

    loop {
        tokio::select! {
            maybe_cmd = manager_rx.recv() => {
                let Some(cmd) = maybe_cmd else { return Err(anyhow::anyhow!("WsClient command channel closed")); };
                match cmd {
                    ManagerCommand::Send(frame) => {
                        ws_writer
                            .send(tokio_tungstenite::tungstenite::Message::text(frame))
                            .await
                            .context("failed to send WS frame")?;
                    }
                }
            }
            maybe_frame = ws_reader.next() => {
                let Some(frame_result) = maybe_frame else { return Err(anyhow::anyhow!("WsClient server closed connection")); };
                let frame = frame_result.context("WsClient read error")?;
                if frame.is_close() {
                    return Err(anyhow::anyhow!("WsClient server sent close frame"));
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
                handle_server_msg(msg, pending, subscribers).await;
            }
        }
    }
}

async fn handle_server_msg(
    msg: ServerMsg,
    pending: &Arc<Mutex<HashMap<String, oneshot::Sender<PendingReply>>>>,
    subscribers: &Arc<Mutex<SubscriberRegistry>>,
) {
    match msg {
        ServerMsg::Published { correlation_id, .. } => {
            if let Some(cid) = correlation_id {
                send_pending(pending, &cid, PendingReply::Published).await;
            }
        }
        ServerMsg::Subscribed { id, correlation_id } => {
            if let Some(cid) = correlation_id {
                if let Some(stable_id) = cid
                    .strip_prefix(RESUBSCRIBE_CID_PREFIX)
                    .and_then(|rest| rest.split_once(':').map(|(stable, _)| stable.to_string()))
                {
                    subscribers.lock().await.bind_server_id(&stable_id, id);
                } else {
                    send_pending(pending, &cid, PendingReply::Subscribed(id)).await;
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
                send_pending(pending, &cid, PendingReply::ReplyResult(decoded)).await;
            }
        }
        ServerMsg::Error {
            message,
            correlation_id,
        } => {
            if let Some(cid) = correlation_id {
                send_pending(pending, &cid, PendingReply::Error(message)).await;
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
            let sender = subscribers
                .lock()
                .await
                .get_sender_by_server_id(&subscription_id);
            if let Some(tx) = sender {
                let _ = tx.send(msg).await;
            }
        }
        ServerMsg::ChannelRegistered { .. }
        | ServerMsg::InterceptorRegistered { .. }
        | ServerMsg::Deliver { .. }
        | ServerMsg::Intercept { .. } => {}
    }
}

async fn send_pending(
    pending: &Arc<Mutex<HashMap<String, oneshot::Sender<PendingReply>>>>,
    cid: &str,
    reply: PendingReply,
) {
    let tx = pending.lock().await.remove(cid);
    if let Some(tx) = tx {
        let _ = tx.send(reply);
    }
}

async fn fail_all_pending(pending: &Arc<Mutex<HashMap<String, oneshot::Sender<PendingReply>>>>) {
    pending.lock().await.clear();
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

    #[tokio::test]
    async fn subscriber_registry_maps_server_ids_to_stable_subscriptions() {
        let (tx, _rx) = mpsc::channel(1);
        let mut registry = SubscriberRegistry::default();
        registry.insert("stable-1".to_string(), "events.>".to_string(), tx.clone());

        assert!(registry.bind_server_id("stable-1", "server-a".to_string()));
        assert!(registry.get_sender_by_server_id("server-a").is_some());
        assert_eq!(
            registry.active_patterns(),
            vec![("stable-1".to_string(), "events.>".to_string())]
        );

        assert!(registry.bind_server_id("stable-1", "server-b".to_string()));
        assert!(registry.get_sender_by_server_id("server-a").is_none());
        assert!(registry.get_sender_by_server_id("server-b").is_some());
    }

    #[tokio::test]
    async fn subscriber_registry_removes_stable_subscription_and_server_mapping() {
        let (tx, _rx) = mpsc::channel(1);
        let mut registry = SubscriberRegistry::default();
        registry.insert("stable-1".to_string(), "events.>".to_string(), tx);
        registry.bind_server_id("stable-1", "server-a".to_string());

        let removed = registry.remove("stable-1").unwrap();

        assert_eq!(removed.pattern, "events.>");
        assert_eq!(removed.server_id, Some("server-a".to_string()));
        assert!(registry.get_sender_by_server_id("server-a").is_none());
        assert!(registry.active_patterns().is_empty());
    }

    #[tokio::test]
    async fn ws_client_reconnects_and_restores_active_subscriptions() {
        use seidrum_eventbus::test_utils::pick_ephemeral_addr;
        use tokio::net::TcpListener;
        use tokio_tungstenite::tungstenite::Message;

        async fn start_scripted_server(
            addr: std::net::SocketAddr,
        ) -> (
            oneshot::Sender<()>,
            mpsc::Sender<&'static [u8]>,
            tokio::task::JoinHandle<()>,
        ) {
            let listener = TcpListener::bind(addr).await.unwrap();
            let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
            let (event_tx, mut event_rx) = mpsc::channel::<&'static [u8]>(8);
            let handle = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let (mut writer, mut reader) = ws.split();
                let mut server_sub_id: Option<String> = None;
                loop {
                    tokio::select! {
                        _ = &mut shutdown_rx => {
                            let _ = writer.send(Message::Close(None)).await;
                            break;
                        }
                        maybe_payload = event_rx.recv() => {
                            let Some(payload) = maybe_payload else { break; };
                            let Some(subscription_id) = server_sub_id.clone() else { continue; };
                            let frame = serde_json::json!({
                                "op": "event",
                                "subject": "reconnect.test",
                                "payload": base64::engine::general_purpose::STANDARD.encode(payload),
                                "reply_subject": null,
                                "subscription_id": subscription_id,
                            });
                            writer.send(Message::text(frame.to_string())).await.unwrap();
                        }
                        maybe_frame = reader.next() => {
                            let Some(Ok(frame)) = maybe_frame else { break; };
                            if !frame.is_text() { continue; }
                            let value: serde_json::Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
                            match value.get("op").and_then(|op| op.as_str()) {
                                Some("subscribe") => {
                                    let cid = value.get("correlation_id").cloned().unwrap_or(serde_json::Value::Null);
                                    let id = format!("server-sub-{}", ulid::Ulid::new());
                                    server_sub_id = Some(id.clone());
                                    let reply = serde_json::json!({"op":"subscribed","id":id,"correlation_id":cid});
                                    writer.send(Message::text(reply.to_string())).await.unwrap();
                                }
                                Some("publish") => {
                                    let cid = value.get("correlation_id").cloned().unwrap_or(serde_json::Value::Null);
                                    let reply = serde_json::json!({"op":"published","seq":1,"correlation_id":cid});
                                    writer.send(Message::text(reply.to_string())).await.unwrap();
                                }
                                _ => {}
                            }
                        }
                    }
                }
            });
            (shutdown_tx, event_tx, handle)
        }

        let addr = pick_ephemeral_addr();
        let (shutdown_a, event_tx_a, server_a) = start_scripted_server(addr).await;

        let client = WsClient::connect_with_reconnect(
            &format!("ws://{addr}"),
            "ws-client-test",
            Duration::from_millis(10),
            Duration::from_millis(50),
            None,
        )
        .await
        .unwrap();
        let mut sub = client.subscribe("reconnect.test").await.unwrap();

        event_tx_a.send(b"before").await.unwrap();
        let before = tokio::time::timeout(Duration::from_secs(2), sub.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.payload.as_ref(), b"before");

        let _ = shutdown_a.send(());
        server_a.await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while client.is_connected() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!client.is_connected());

        let (shutdown_b, event_tx_b, server_b) = start_scripted_server(addr).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !client.is_connected() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(client.is_connected());

        event_tx_b.send(b"after").await.unwrap();
        let after = tokio::time::timeout(Duration::from_secs(2), sub.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.payload.as_ref(), b"after");

        let _ = shutdown_b.send(());
        server_b.await.unwrap();
    }
}
