//! Backend-agnostic bus client used by every Seidrum plugin and kernel
//! service.
//!
//! `BusClient` supports two backends:
//! - **In-process** (`BusClient::from_bus`) — wraps `Arc<dyn EventBus>`
//!   directly. Used by the kernel so services talk to the bus without
//!   a network round-trip.
//! - **WebSocket** (`BusClient::connect("ws://...")`) — wraps
//!   [`crate::ws_client::WsClient`]. Used by plugins running as
//!   separate processes that connect to the kernel's WS transport.

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info};

use crate::events::EventEnvelope;
use crate::ws_client::{WsClient, WsMessage, WsSubject, WsSubscription};

// === Consumer-facing type aliases ===

/// A message received from the bus.
pub type Message = WsMessage;

/// A subscription handle. Call `.next().await` to receive [`Message`]s.
pub type Subscription = WsSubscription;

/// A subject string.
pub type Subject = WsSubject;

/// Arc-wrapped backend so `BusClient` is `Clone`.
#[derive(Clone)]
enum BackendHandle {
    Ws(WsClient),
    InProcess(Arc<dyn seidrum_eventbus::EventBus>),
}

/// Backend-agnostic client for talking to the Seidrum bus.
#[derive(Clone)]
pub struct BusClient {
    backend: BackendHandle,
    /// Default source identifier stamped onto envelopes.
    pub source: String,
}

impl BusClient {
    /// Maximum number of connection attempts before giving up.
    const CONNECT_MAX_ATTEMPTS: u32 = 20;
    /// Initial retry delay (doubles each attempt, capped at 5s).
    const CONNECT_INITIAL_BACKOFF_MS: u64 = 250;
    /// Maximum retry delay.
    const CONNECT_MAX_BACKOFF_MS: u64 = 5000;

    /// Connect to the bus via WebSocket, retrying with exponential
    /// backoff if the server isn't up yet.
    ///
    /// This handles the common startup-ordering scenario where a
    /// plugin process starts before the kernel has finished binding
    /// its WS port. With the default 20 attempts × 250ms→5s backoff,
    /// the plugin will wait up to ~30s for the kernel to appear.
    ///
    /// `url` must be `ws://` or `wss://`.
    pub async fn connect(url: &str, source: &str) -> Result<Self> {
        if !url.starts_with("ws://") && !url.starts_with("wss://") {
            return Err(anyhow::anyhow!(
                "unsupported bus URL scheme: {url} (expected ws:// or wss://)"
            ));
        }

        let mut attempt = 0u32;
        let mut backoff_ms = Self::CONNECT_INITIAL_BACKOFF_MS;

        loop {
            attempt += 1;
            match WsClient::connect(url, source).await {
                Ok(ws) => {
                    if attempt > 1 {
                        info!(
                            url,
                            source,
                            attempts = attempt,
                            "connected to bus after retries"
                        );
                    }
                    return Ok(Self {
                        backend: BackendHandle::Ws(ws),
                        source: source.to_string(),
                    });
                }
                Err(e) => {
                    if attempt >= Self::CONNECT_MAX_ATTEMPTS {
                        return Err(e.context(format!(
                            "failed to connect to bus at {} after {} attempts",
                            url, attempt
                        )));
                    }
                    debug!(
                        url,
                        source,
                        attempt,
                        backoff_ms,
                        error = %e,
                        "bus not ready, retrying..."
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(Self::CONNECT_MAX_BACKOFF_MS);
                }
            }
        }
    }

    /// Create a `BusClient` backed by an in-process `EventBus`.
    pub fn from_bus(bus: Arc<dyn seidrum_eventbus::EventBus>, source: &str) -> Self {
        info!(source, "BusClient created (in-process backend)");
        Self {
            backend: BackendHandle::InProcess(bus),
            source: source.to_string(),
        }
    }

    /// Publish a serializable payload to a subject.
    pub async fn publish<T: Serialize>(&self, subject: impl AsRef<str>, payload: &T) -> Result<()> {
        let bytes = serde_json::to_vec(payload).context("failed to serialize payload")?;
        self.publish_bytes(subject, bytes).await
    }

    /// Publish a raw byte payload to a subject.
    pub async fn publish_bytes(
        &self,
        subject: impl AsRef<str>,
        payload: impl Into<bytes::Bytes>,
    ) -> Result<()> {
        let subject_str = subject.as_ref();
        let bytes: bytes::Bytes = payload.into();
        match &self.backend {
            BackendHandle::Ws(ws) => {
                ws.publish_bytes(subject_str, bytes.as_ref()).await?;
            }
            BackendHandle::InProcess(bus) => {
                bus.publish(subject_str, &bytes)
                    .await
                    .map_err(|e| anyhow::anyhow!("publish failed: {}", e))?;
                debug!(subject = subject_str, "published message (in-process)");
            }
        }
        Ok(())
    }

    /// Publish a payload wrapped in an [`EventEnvelope`].
    pub async fn publish_envelope<T: Serialize>(
        &self,
        subject: &str,
        correlation_id: Option<String>,
        scope: Option<String>,
        payload: &T,
    ) -> Result<EventEnvelope> {
        let envelope = EventEnvelope::new(subject, &self.source, correlation_id, scope, payload)
            .context("failed to build EventEnvelope")?;
        self.publish(subject, &envelope).await?;
        Ok(envelope)
    }

    /// Send a request and deserialize the typed response.
    pub async fn request<T: Serialize, R: DeserializeOwned>(
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

    /// Send a raw byte request and return the raw byte response.
    pub async fn request_bytes(
        &self,
        subject: impl AsRef<str>,
        payload: impl Into<bytes::Bytes>,
    ) -> Result<Vec<u8>> {
        let subject_str = subject.as_ref();
        let bytes: bytes::Bytes = payload.into();
        match &self.backend {
            BackendHandle::Ws(ws) => ws.request_bytes(subject_str, bytes.as_ref()).await,
            BackendHandle::InProcess(bus) => {
                let reply = bus
                    .request(subject_str, &bytes, Duration::from_secs(5))
                    .await
                    .map_err(|e| anyhow::anyhow!("request failed: {}", e))?;
                Ok(reply)
            }
        }
    }

    /// Subscribe to a subject pattern.
    pub async fn subscribe(&self, subject: impl AsRef<str>) -> Result<Subscription> {
        let subject_str = subject.as_ref();
        match &self.backend {
            BackendHandle::Ws(ws) => ws.subscribe(subject_str).await,
            BackendHandle::InProcess(bus) => {
                let opts = seidrum_eventbus::SubscribeOpts {
                    priority: 10,
                    mode: seidrum_eventbus::SubscriptionMode::Async,
                    channel: seidrum_eventbus::ChannelConfig::InProcess,
                    timeout: Duration::from_secs(5),
                    filter: None,
                };
                let sub = bus
                    .subscribe(subject_str, opts)
                    .await
                    .map_err(|e| anyhow::anyhow!("subscribe failed: {}", e))?;
                let sub_id = sub.id.clone();

                // Bridge: DispatchedEvent → WsMessage
                let (tx, rx) = tokio::sync::mpsc::channel::<WsMessage>(256);
                let mut event_rx = sub.rx;
                tokio::spawn(async move {
                    while let Some(event) = event_rx.recv().await {
                        let msg = WsMessage {
                            subject: event.subject,
                            payload: bytes::Bytes::from(event.payload),
                            reply: event.reply_subject,
                        };
                        if tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                });

                Ok(WsSubscription { rx, id: sub_id })
            }
        }
    }

    /// Reply to a captured reply subject.
    pub async fn reply_to(
        &self,
        reply_subject: &Subject,
        payload: impl Into<bytes::Bytes>,
    ) -> Result<()> {
        self.publish_bytes(reply_subject.as_str(), payload).await
    }

    /// Returns `true` if the bus connection is healthy.
    pub fn is_connected(&self) -> bool {
        match &self.backend {
            BackendHandle::Ws(ws) => ws.is_connected(),
            BackendHandle::InProcess(_) => true,
        }
    }
}
