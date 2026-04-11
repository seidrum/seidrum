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
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Maximum number of subscriptions a single WebSocket connection may hold.
const MAX_SUBSCRIPTIONS_PER_CONNECTION: usize = 64;

/// Maximum number of remote interceptors a single WS connection may register.
/// Stricter than the subscription cap because every event flowing through the
/// bus pays for each registered interceptor.
const MAX_INTERCEPTORS_PER_CONNECTION: usize = 16;

/// Maximum size of a single incoming WebSocket text frame (2 MiB).
const MAX_FRAME_SIZE: usize = 2_097_152;

/// Authentication request context provided to the [`Authenticator`].
///
/// Contains the peer address and any HTTP headers from the WebSocket upgrade
/// request, allowing token-based or header-based authentication.
pub struct AuthRequest {
    pub peer_addr: SocketAddr,
    pub headers: HashMap<String, String>,
}

/// Authentication trait for WebSocket connections.
///
/// Implement this to add authentication to the WebSocket transport server.
/// The authenticator is called once per connection during the upgrade handshake.
#[async_trait::async_trait]
pub trait Authenticator: Send + Sync + 'static {
    /// Authenticate a connection using the upgrade request context.
    /// Returns `Ok(())` if allowed, or `Err(reason)` if rejected.
    async fn authenticate(&self, request: &AuthRequest) -> Result<(), String>;
}

/// No-op authenticator that accepts all connections (development only).
pub struct NoAuth;

#[async_trait::async_trait]
impl Authenticator for NoAuth {
    async fn authenticate(&self, _request: &AuthRequest) -> Result<(), String> {
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
    /// Register a custom delivery channel type backed by this connection.
    ///
    /// The server installs a proxy [`crate::delivery::DeliveryChannel`] in
    /// the bus's channel registry under `channel_type`. Subsequent
    /// subscriptions that use `ChannelConfig::Custom { channel_type, .. }`
    /// will have their events forwarded over this WebSocket connection
    /// via [`ServerMessage::Deliver`] frames.
    ///
    /// The client is expected to respond to each `deliver` message with a
    /// matching [`ClientOperation::DeliverResult`] indicating success or
    /// failure. If the client disconnects, the proxy channel fails all
    /// pending deliveries (which then enter the retry queue per the
    /// normal Phase 5 retry path).
    #[serde(rename = "register_channel_type")]
    RegisterChannelType {
        channel_type: String,
        #[serde(default)]
        correlation_id: Option<String>,
    },
    /// Acknowledgment of a delivery sent via [`ServerMessage::Deliver`].
    /// `request_id` matches the value the server sent.
    #[serde(rename = "deliver_result")]
    DeliverResult {
        request_id: String,
        /// `true` if the client successfully processed the event.
        success: bool,
        /// Optional error message when `success = false`.
        #[serde(default)]
        error: Option<String>,
    },
    /// Register a remote sync interceptor.
    ///
    /// The server installs a proxy [`crate::dispatch::Interceptor`] in the
    /// bus's interceptor chain at the requested priority and pattern.
    /// Subsequent events whose subjects match `pattern` will be forwarded
    /// to the client via [`ServerMessage::Intercept`] frames; the client
    /// must respond with [`ClientOperation::InterceptResult`] using the
    /// matching `request_id`.
    ///
    /// If the client does not respond within `timeout_ms` (default 5000ms),
    /// the proxy returns `Pass` so the dispatch chain continues. On
    /// disconnect, all in-flight intercept calls are cancelled with `Pass`.
    #[serde(rename = "register_interceptor")]
    RegisterInterceptor {
        pattern: String,
        #[serde(default)]
        priority: u32,
        #[serde(default)]
        timeout_ms: Option<u64>,
        #[serde(default)]
        correlation_id: Option<String>,
    },
    /// Acknowledgment of an [`ServerMessage::Intercept`] frame. The
    /// `action` field is a tagged enum so the `payload` is required iff
    /// `action == "modify"` — malformed frames are caught at parse time
    /// rather than producing a 5s dispatch stall.
    #[serde(rename = "intercept_result")]
    InterceptResult {
        request_id: String,
        #[serde(flatten)]
        action: WireInterceptAction,
    },
}

/// Wire-level intercept action returned by a remote sync interceptor.
///
/// Tagged on the `action` field so serde enforces "payload must be present
/// when modify" at deserialise time. Intentionally distinct from the
/// in-process [`crate::dispatch::InterceptResult`] (which carries no
/// payload — the payload is mutated through `&mut Vec<u8>`).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum WireInterceptAction {
    /// Continue dispatch unchanged.
    Pass,
    /// Drop the event — async subscribers do not see it.
    Drop,
    /// Replace the payload with `payload` (base64-encoded) and continue
    /// dispatch with the new bytes.
    Modify { payload: String },
}

fn default_timeout_ms() -> u64 {
    super::DEFAULT_TIMEOUT_MS
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
    /// Acknowledgment of a successful publish.
    #[serde(rename = "published")]
    Published {
        seq: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        correlation_id: Option<String>,
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
    /// Confirmation of a successful `register_channel_type`.
    #[serde(rename = "channel_registered")]
    ChannelRegistered {
        channel_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        correlation_id: Option<String>,
    },
    /// Server-initiated delivery via a remote channel proxy. The client
    /// must reply with [`ClientOperation::DeliverResult`] using the same
    /// `request_id`.
    #[serde(rename = "deliver")]
    Deliver {
        request_id: String,
        channel_type: String,
        subject: String,
        /// Base64-encoded payload.
        payload: String,
    },
    /// Confirmation of a successful `register_interceptor`.
    #[serde(rename = "interceptor_registered")]
    InterceptorRegistered {
        /// Bus subscription id for the registered interceptor. The client
        /// uses this to unsubscribe later via [`ClientOperation::Unsubscribe`].
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        correlation_id: Option<String>,
    },
    /// Server-initiated intercept call. The client must inspect the
    /// payload and reply with [`ClientOperation::InterceptResult`] using
    /// the same `request_id`.
    #[serde(rename = "intercept")]
    Intercept {
        request_id: String,
        subject: String,
        /// Base64-encoded payload.
        payload: String,
    },
}

/// WebSocket server for remote event bus clients.
///
/// Call [`WebSocketServer::start`] to begin accepting connections. The server
/// runs until the shutdown signal is received or an error occurs.
pub struct WebSocketServer {
    bus: Arc<dyn EventBus>,
    authenticator: Arc<dyn Authenticator>,
    shutdown_rx: watch::Receiver<bool>,
}

impl WebSocketServer {
    /// Create a new WebSocket server with no authentication (development only).
    pub fn new(bus: Arc<dyn EventBus>, shutdown_rx: watch::Receiver<bool>) -> Self {
        Self {
            bus,
            authenticator: Arc::new(NoAuth),
            shutdown_rx,
        }
    }

    /// Create a new WebSocket server with a custom authenticator.
    pub fn with_auth(
        bus: Arc<dyn EventBus>,
        auth: Arc<dyn Authenticator>,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            bus,
            authenticator: auth,
            shutdown_rx,
        }
    }

    /// Start the WebSocket server on the given address.
    ///
    /// This method runs until the shutdown signal is received or a fatal error
    /// occurs. Returns `Err` only if the initial bind fails.
    pub async fn start(&self, addr: SocketAddr) -> crate::Result<()> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| crate::EventBusError::Internal(format!("Failed to bind: {}", e)))?;

        info!("WebSocket server listening on {}", addr);

        let mut shutdown_rx = self.shutdown_rx.clone();

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    let (socket, peer_addr) = accept_result
                        .map_err(|e| crate::EventBusError::Internal(format!("Accept failed: {}", e)))?;

                    let bus = Arc::clone(&self.bus);
                    let auth = Arc::clone(&self.authenticator);
                    tokio::spawn(async move {
                        // Capture HTTP upgrade headers via accept_hdr_async's callback,
                        // then authenticate before serving any messages.
                        let captured_headers: Arc<std::sync::Mutex<HashMap<String, String>>> =
                            Arc::new(std::sync::Mutex::new(HashMap::new()));
                        let captured_clone = Arc::clone(&captured_headers);

                        // H3 fix: cap message and frame sizes at the wire
                        // layer so tungstenite rejects oversized payloads
                        // before allocating its 64 MiB default buffer.
                        // The app-level MAX_FRAME_SIZE check at the read
                        // loop is a defence-in-depth backup.
                        let ws_config =
                            tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
                                .max_message_size(Some(MAX_FRAME_SIZE))
                                .max_frame_size(Some(MAX_FRAME_SIZE));
                        let upgrade = tokio_tungstenite::accept_hdr_async_with_config(
                            socket,
                            #[allow(clippy::result_large_err)]
                            move |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                                  resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
                                let mut map = captured_clone.lock().unwrap();
                                for (name, value) in req.headers().iter() {
                                    if let Ok(v) = value.to_str() {
                                        map.insert(name.as_str().to_string(), v.to_string());
                                    }
                                }
                                Ok(resp)
                            },
                            Some(ws_config),
                        )
                        .await;

                        let ws = match upgrade {
                            Ok(ws) => ws,
                            Err(e) => {
                                warn!("WebSocket upgrade failed from {}: {}", peer_addr, e);
                                return;
                            }
                        };

                        let headers = std::mem::take(&mut *captured_headers.lock().unwrap());
                        let auth_req = AuthRequest { peer_addr, headers };
                        if let Err(reason) = auth.authenticate(&auth_req).await {
                            warn!(
                                "WebSocket connection rejected from {}: {}",
                                peer_addr, reason
                            );
                            // Drop the upgraded socket without serving messages.
                            return;
                        }

                        if let Err(e) = serve_connection(ws, peer_addr, bus).await {
                            warn!(
                                "Error handling WebSocket connection from {}: {}",
                                peer_addr, e
                            );
                        }
                    });
                }
                _ = shutdown_rx.wait_for(|&v| v) => {
                    info!("WebSocket server shutting down");
                    break;
                }
            }
        }

        Ok(())
    }
}

/// Per-connection subscription bookkeeping.
struct ConnectionSubscription {
    pattern: String,
    /// Handle to the forwarder task; aborted on disconnect cleanup.
    forwarder: JoinHandle<()>,
}

/// Per-connection state for `WsRemoteChannel` proxies registered by this client.
struct RegisteredRemoteChannel {
    /// The channel type name (kept for logging and future deregistration support).
    #[allow(dead_code)]
    channel_type: String,
    /// Shared pending-replies map. The connection's read loop pushes
    /// `DeliverResult` replies into this map; the channel's `deliver()`
    /// awaits them via oneshot.
    proxy: Arc<crate::delivery::WsRemoteChannel>,
}

/// Per-connection state for `WsRemoteInterceptor` proxies registered by
/// this client. Mirrors [`RegisteredRemoteChannel`] but for the interceptor
/// chain instead of delivery channels.
struct RegisteredRemoteInterceptor {
    /// Pattern this interceptor was registered against (logging/debug).
    #[allow(dead_code)]
    pattern: String,
    /// Bus subscription id, used to unsubscribe on disconnect.
    bus_id: String,
    /// Shared pending-replies map. The read loop pushes `intercept_result`
    /// messages here; `intercept()` awaits them via oneshot.
    proxy: Arc<crate::delivery::WsRemoteInterceptor>,
}

async fn serve_connection(
    ws: tokio_tungstenite::WebSocketStream<TcpStream>,
    peer_addr: std::net::SocketAddr,
    bus: Arc<dyn EventBus>,
) -> crate::Result<()> {
    debug!("WebSocket client connected: {}", peer_addr);

    let (mut sender, mut receiver) = ws.split();
    let mut subscriptions: HashMap<String, ConnectionSubscription> = HashMap::new();
    // Channel-type → registered remote channel proxy. Used so the read
    // loop can route DeliverResult messages and so disconnect cleanup
    // can unregister these channels from the bus.
    let mut remote_channels: HashMap<String, RegisteredRemoteChannel> = HashMap::new();
    // Bus interceptor id → registered remote interceptor proxy. The read
    // loop walks this map to route `intercept_result` messages back to the
    // proxy that issued the corresponding `intercept` frame.
    let mut remote_interceptors: HashMap<String, RegisteredRemoteInterceptor> = HashMap::new();

    // Channel for forwarding subscription events AND outbound deliver
    // frames to the WebSocket sender. Subscription receivers are polled
    // in forwarder tasks; remote channel proxies push deliver frames
    // here as well.
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
                            &mut subscriptions,
                            &mut remote_channels,
                            &mut remote_interceptors,
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

    // === Disconnect cleanup ===
    // 1. Abort all forwarder tasks immediately (prevents them from racing
    //    against channel closure during cleanup).
    // 2. Unsubscribe from the bus to release trie entries and senders.
    let sub_ids: Vec<String> = subscriptions.keys().cloned().collect();
    for sub_id in &sub_ids {
        if let Some(conn_sub) = subscriptions.remove(sub_id) {
            conn_sub.forwarder.abort();
        }
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

    // Cancel any in-flight remote channel deliveries and unregister the
    // proxies from the bus's channel registry. The cancellation makes
    // pending `deliver()` calls fail promptly so the failed deliveries
    // enter the retry queue per the normal Phase 5 path.
    if !remote_channels.is_empty() {
        for entry in remote_channels.values() {
            entry.proxy.cancel_all("WebSocket connection closed").await;
        }
        // Unregister from the registry. Use the dispatch engine via the
        // bus to access the registry; we don't have direct access here.
        // Best-effort: the engine's channel_registry is internal, so we
        // rely on the registry's "replace silently if exists" semantics
        // — once cancelled, future delivery attempts via the proxy will
        // fail (outbound channel is closed), and the retry task will
        // dead-letter them as Permanent.
        debug!(
            "Cancelled {} remote channel proxies for disconnected client {}",
            remote_channels.len(),
            peer_addr
        );
    }

    // Cancel and unregister remote interceptor proxies. **Order matters
    // (M3 fix):** unsubscribe from the bus FIRST so the dispatch engine
    // can no longer find the proxy in the trie, THEN cancel pending
    // intercept calls. The reverse order admits new intercept calls
    // that race in between cancel_all and bus.unsubscribe — those new
    // calls would never see the cancellation.
    if !remote_interceptors.is_empty() {
        let interceptor_ids: Vec<String> = remote_interceptors.keys().cloned().collect();
        for id in &interceptor_ids {
            if let Err(e) = bus.unsubscribe(id).await {
                debug!(
                    "Failed to unregister remote interceptor {} on disconnect from {}: {}",
                    id, peer_addr, e
                );
            }
        }
        for entry in remote_interceptors.values() {
            entry
                .proxy
                .cancel_all("WebSocket connection closed")
                .await;
        }
        debug!(
            "Cleaned up {} remote interceptor proxies for disconnected client {}",
            interceptor_ids.len(),
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

/// Validate and decode a base64 payload, mapping to our error type.
fn validate_and_decode_payload(payload: &str) -> crate::Result<Vec<u8>> {
    super::validate_and_decode_payload(payload).map_err(|msg| {
        if msg.contains("exceeds limit") {
            crate::EventBusError::PayloadTooLarge(msg)
        } else {
            crate::EventBusError::Internal(msg)
        }
    })
}

async fn handle_operation(
    bus: &Arc<dyn EventBus>,
    subscriptions: &mut HashMap<String, ConnectionSubscription>,
    remote_channels: &mut HashMap<String, RegisteredRemoteChannel>,
    remote_interceptors: &mut HashMap<String, RegisteredRemoteInterceptor>,
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
            correlation_id,
        } => {
            let decoded = validate_and_decode_payload(&payload)?;
            let seq = bus.publish(&subject, &decoded).await?;
            debug!("Published to {} seq={}", subject, seq);

            // Send publish acknowledgment with sequence number
            let ack = ServerMessage::Published {
                seq,
                correlation_id,
            };
            if let Ok(json) = serde_json::to_string(&ack) {
                ws_send(sender, json).await?;
            }
        }

        ClientOperation::Subscribe {
            pattern,
            opts,
            correlation_id,
        } => {
            // Enforce per-connection subscription limit
            if subscriptions.len() >= MAX_SUBSCRIPTIONS_PER_CONNECTION {
                return Err(crate::EventBusError::Internal(format!(
                    "Subscription limit reached ({} max per connection)",
                    MAX_SUBSCRIPTIONS_PER_CONNECTION
                )));
            }

            let subscribe_opts = SubscribeOpts {
                priority: opts.priority,
                mode: SubscriptionMode::Async,
                channel: ChannelConfig::WebSocket,
                timeout: Duration::from_secs(5),
                filter: None,
            };

            let sub = bus.subscribe(&pattern, subscribe_opts).await?;
            let sub_id = sub.id.clone();

            // Spawn a forwarder task: reads events from the subscription receiver
            // and sends them to the shared forward channel for the main select! loop.
            //
            // The task exits when either:
            // - rx.recv() returns None (bus unsubscribed, dropping the tx side)
            // - forward_tx_clone.send() fails (connection closed, forward_rx dropped)
            // - The JoinHandle is aborted during disconnect cleanup
            let forward_tx_clone = forward_tx.clone();
            let sub_id_for_task = sub_id.clone();
            let mut rx = sub.rx;
            let forwarder = tokio::spawn(async move {
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
                            break;
                        }
                    }
                }
            });

            subscriptions.insert(
                sub_id.clone(),
                ConnectionSubscription {
                    pattern: pattern.clone(),
                    forwarder,
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
                conn_sub.forwarder.abort();
                bus.unsubscribe(&id).await?;
                debug!(
                    "Unsubscribed from pattern: {} (id={})",
                    conn_sub.pattern, id
                );
            } else if let Some(entry) = remote_interceptors.remove(&id) {
                // Also handles unsubscribing remote interceptors so the
                // client can use the same op to detach either subscription
                // type. Cancel any pending replies first so in-flight
                // intercept calls return Pass instead of hanging.
                entry.proxy.cancel_all("interceptor unregistered").await;
                bus.unsubscribe(&entry.bus_id).await?;
                debug!(
                    "Unregistered remote interceptor (id={}, pattern={})",
                    id, entry.pattern
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

        ClientOperation::RegisterChannelType {
            channel_type,
            correlation_id,
        } => {
            // Construct a proxy channel that forwards events over this
            // connection's outbound queue. The proxy is registered in the
            // bus's channel registry; subscriptions that use Custom { type }
            // matching this name will route through it.
            let proxy = Arc::new(crate::delivery::WsRemoteChannel::new(
                channel_type.clone(),
                forward_tx.clone(),
            ));
            bus.register_channel_type(
                &channel_type,
                Arc::clone(&proxy) as Arc<dyn crate::delivery::DeliveryChannel>,
            )
            .await?;

            remote_channels.insert(
                channel_type.clone(),
                RegisteredRemoteChannel {
                    channel_type: channel_type.clone(),
                    proxy,
                },
            );

            debug!("Registered remote channel type: {}", channel_type);

            let response = ServerMessage::ChannelRegistered {
                channel_type,
                correlation_id,
            };
            if let Ok(json) = serde_json::to_string(&response) {
                ws_send(sender, json).await?;
            }
        }

        ClientOperation::DeliverResult {
            request_id,
            success,
            error,
        } => {
            // Route the reply to whichever remote channel proxy is awaiting
            // it. Since channel types are namespaced per-connection, we
            // try each registered proxy until one signals the reply.
            let mut signaled = false;
            for entry in remote_channels.values() {
                let mut pending = entry.proxy.pending_replies().lock_owned().await;
                if let Some(tx) = pending.remove(&request_id) {
                    let _ = tx.send(crate::delivery::WsDeliveryReply {
                        success,
                        error: error.clone(),
                    });
                    signaled = true;
                    break;
                }
            }
            if !signaled {
                debug!(request_id = %request_id, "DeliverResult had no matching pending delivery");
            }
        }

        ClientOperation::RegisterInterceptor {
            pattern,
            priority,
            timeout_ms,
            correlation_id,
        } => {
            // === C1 / B1 hardening ===
            // Token-aware pattern validation: rejects '>', '*.*', '*.>',
            // '_reply.*', leading whitespace, etc. Shared between WS and
            // HTTP so the policies cannot drift.
            if let Err(e) = crate::transport::validate_remote_interceptor_pattern(&pattern) {
                return Err(crate::EventBusError::InvalidSubject(e.to_string()));
            }
            // Reserve low priorities for trusted in-process interceptors.
            let effective_priority =
                crate::transport::clamp_remote_interceptor_priority(priority);
            // Cap the per-call timeout to prevent slowloris stalls of
            // the dispatch chain.
            let clamped_timeout_ms =
                crate::transport::clamp_remote_interceptor_timeout(timeout_ms);
            // Per-connection cap so a single client can't fill the
            // interceptor table for the whole bus.
            if remote_interceptors.len() >= MAX_INTERCEPTORS_PER_CONNECTION {
                return Err(crate::EventBusError::Internal(format!(
                    "interceptor limit reached ({} max per WS connection)",
                    MAX_INTERCEPTORS_PER_CONNECTION
                )));
            }

            // Build the proxy interceptor and register it with the bus.
            // Use a proxy timeout slightly shorter than the engine
            // timeout (H1 fix) so the proxy's PendingGuard reclaims the
            // pending entry before the engine's abort_handle fires.
            let engine_timeout = clamped_timeout_ms.map(Duration::from_millis);
            let proxy_timeout = engine_timeout
                .map(|d| d.saturating_sub(Duration::from_millis(250)))
                .map(|d| d.max(Duration::from_millis(100)));
            let mut proxy = crate::delivery::WsRemoteInterceptor::new(
                pattern.clone(),
                forward_tx.clone(),
            );
            if let Some(t) = proxy_timeout {
                proxy = proxy.with_timeout(t);
            }
            let proxy = Arc::new(proxy);

            let bus_id = bus
                .intercept(
                    &pattern,
                    effective_priority,
                    Arc::clone(&proxy) as Arc<dyn crate::dispatch::Interceptor>,
                    engine_timeout,
                )
                .await?;

            remote_interceptors.insert(
                bus_id.clone(),
                RegisteredRemoteInterceptor {
                    pattern: pattern.clone(),
                    bus_id: bus_id.clone(),
                    proxy,
                },
            );

            debug!(
                "Registered remote interceptor on pattern {} (id={})",
                pattern, bus_id
            );

            let response = ServerMessage::InterceptorRegistered {
                id: bus_id,
                correlation_id,
            };
            if let Ok(json) = serde_json::to_string(&response) {
                ws_send(sender, json).await?;
            }
        }

        ClientOperation::InterceptResult { request_id, action } => {
            // Translate the wire-level action into the internal struct.
            // Serde already enforced "modify implies payload"; we only
            // need to validate the base64 here.
            let parsed_action = match action {
                WireInterceptAction::Pass => crate::delivery::WsInterceptAction::Pass,
                WireInterceptAction::Drop => crate::delivery::WsInterceptAction::Drop,
                WireInterceptAction::Modify { payload } => {
                    match base64::engine::general_purpose::STANDARD.decode(&payload) {
                        Ok(bytes) => crate::delivery::WsInterceptAction::Modify(bytes),
                        Err(e) => {
                            warn!(
                                request_id = %request_id,
                                error = %e,
                                "intercept_result modify payload not valid base64; ignoring"
                            );
                            return Ok(());
                        }
                    }
                }
            };

            // Route the reply to whichever interceptor proxy is awaiting
            // it. Only one proxy will own the request_id, so we move the
            // action into the first match.
            let mut reply_slot = Some(crate::delivery::WsInterceptReply {
                action: parsed_action,
                error: None,
            });
            let mut signaled = false;
            for entry in remote_interceptors.values() {
                let mut pending = entry.proxy.pending_replies().lock_owned().await;
                if let Some(tx) = pending.remove(&request_id) {
                    if let Some(reply) = reply_slot.take() {
                        let _ = tx.send(reply);
                    }
                    signaled = true;
                    break;
                }
            }
            if !signaled {
                debug!(
                    request_id = %request_id,
                    "intercept_result had no matching pending intercept call"
                );
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
    fn test_parse_register_channel_type() {
        let json = json!({
            "op": "register_channel_type",
            "channel_type": "mqtt",
            "correlation_id": "reg-1"
        });
        let op: ClientOperation = serde_json::from_value(json).unwrap();
        match op {
            ClientOperation::RegisterChannelType {
                channel_type,
                correlation_id,
            } => {
                assert_eq!(channel_type, "mqtt");
                assert_eq!(correlation_id, Some("reg-1".to_string()));
            }
            _ => panic!("wrong op"),
        }
    }

    #[test]
    fn test_parse_deliver_result() {
        let json = json!({
            "op": "deliver_result",
            "request_id": "req-42",
            "success": true
        });
        let op: ClientOperation = serde_json::from_value(json).unwrap();
        match op {
            ClientOperation::DeliverResult {
                request_id,
                success,
                error,
            } => {
                assert_eq!(request_id, "req-42");
                assert!(success);
                assert!(error.is_none());
            }
            _ => panic!("wrong op"),
        }
    }

    #[test]
    fn test_server_message_deliver_serializes() {
        let msg = ServerMessage::Deliver {
            request_id: "r1".to_string(),
            channel_type: "mqtt".to_string(),
            subject: "iot.sensor.temp".to_string(),
            payload: "aGVsbG8=".to_string(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["op"], "deliver");
        assert_eq!(json["request_id"], "r1");
        assert_eq!(json["channel_type"], "mqtt");
        assert_eq!(json["subject"], "iot.sensor.temp");
    }

    #[test]
    fn test_server_message_channel_registered_serializes() {
        let msg = ServerMessage::ChannelRegistered {
            channel_type: "mqtt".to_string(),
            correlation_id: Some("reg-1".to_string()),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["op"], "channel_registered");
        assert_eq!(json["channel_type"], "mqtt");
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
    fn test_server_message_published() {
        let msg = ServerMessage::Published {
            seq: 42,
            correlation_id: Some("req-1".to_string()),
        };

        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["op"], "published");
        assert_eq!(json["seq"], 42);
        assert_eq!(json["correlation_id"], "req-1");
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
        let huge = "A".repeat(super::super::MAX_PAYLOAD_SIZE + 1);
        let result = validate_and_decode_payload(&huge);
        assert!(result.is_err());
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

    // === N8b: WireInterceptAction parser tests ===

    #[test]
    fn test_parse_intercept_result_pass() {
        let json = json!({
            "op": "intercept_result",
            "request_id": "r1",
            "action": "pass"
        });
        let op: ClientOperation = serde_json::from_value(json).unwrap();
        match op {
            ClientOperation::InterceptResult { request_id, action } => {
                assert_eq!(request_id, "r1");
                assert!(matches!(action, WireInterceptAction::Pass));
            }
            _ => panic!("wrong op"),
        }
    }

    #[test]
    fn test_parse_intercept_result_drop() {
        let json = json!({
            "op": "intercept_result",
            "request_id": "r1",
            "action": "drop"
        });
        let op: ClientOperation = serde_json::from_value(json).unwrap();
        assert!(matches!(
            op,
            ClientOperation::InterceptResult {
                action: WireInterceptAction::Drop,
                ..
            }
        ));
    }

    #[test]
    fn test_parse_intercept_result_modify_with_payload() {
        let json = json!({
            "op": "intercept_result",
            "request_id": "r1",
            "action": "modify",
            "payload": "aGVsbG8="
        });
        let op: ClientOperation = serde_json::from_value(json).unwrap();
        match op {
            ClientOperation::InterceptResult {
                action: WireInterceptAction::Modify { payload },
                ..
            } => {
                assert_eq!(payload, "aGVsbG8=");
            }
            _ => panic!("expected Modify"),
        }
    }

    #[test]
    fn test_parse_intercept_result_modify_without_payload_rejected() {
        // serde must enforce the modify-implies-payload invariant at
        // parse time. Previously this was a runtime check that left the
        // pending intercept call hanging until its 5s timeout.
        let json = json!({
            "op": "intercept_result",
            "request_id": "r1",
            "action": "modify"
        });
        let result: Result<ClientOperation, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_intercept_result_unknown_action_rejected() {
        let json = json!({
            "op": "intercept_result",
            "request_id": "r1",
            "action": "explode"
        });
        let result: Result<ClientOperation, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }
}
