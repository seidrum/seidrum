//! Backend-agnostic bus client used by every Seidrum plugin and kernel
//! service.
//!
//! `BusClient` dispatches on the URL scheme at connect time:
//! - `nats://` → wraps `async_nats::Client` (current production backend)
//! - `ws://` or `wss://` → wraps [`crate::ws_client::WsClient`]
//!   (eventbus WebSocket transport — Phase 5 migration target)
//!
//! All consumer-facing types ([`Subscription`], [`Message`], [`Subject`])
//! are the same regardless of backend, so plugin source code does not
//! change when the kernel switches from NATS to the eventbus.
//!
//! **Do not** add an `inner()` escape hatch. That kind of leak is
//! exactly the technical debt the PR #30 rename was created to remove.

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use tracing::{debug, info};

use crate::events::EventEnvelope;
use crate::ws_client::{WsClient, WsMessage, WsSubject, WsSubscription};

// === Consumer-facing type aliases ===
//
// These are now concrete types from ws_client, not async_nats aliases.
// Both backends produce these same types — the NATS backend converts
// internally. Phase 5 Step 3 will remove the NATS backend entirely;
// until then both work side by side.

/// A message received from the bus. Access `.subject`, `.payload`,
/// `.reply` exactly like the old `async_nats::Message`.
pub type Message = WsMessage;

/// A subscription handle. Call `.next().await` to receive [`Message`]s.
pub type Subscription = WsSubscription;

/// A subject string. Just a `String` — can be cloned, displayed, and
/// passed to `reply_to`.
pub type Subject = WsSubject;

/// The active backend.
enum Backend {
    Nats(async_nats::Client),
    Ws(WsClient),
}

/// Backend-agnostic client for talking to the Seidrum bus.
///
/// Created via [`BusClient::connect`]. Dispatches on URL scheme:
/// `nats://` uses the NATS backend, `ws://`/`wss://` uses the
/// eventbus WS transport.
#[derive(Clone)]
pub struct BusClient {
    backend: BackendHandle,
    /// Default source identifier used when wrapping payloads in
    /// envelopes via [`Self::publish_envelope`]. Set at connect time.
    pub source: String,
}

/// Arc-wrapped backend so `BusClient` is `Clone`.
#[derive(Clone)]
enum BackendHandle {
    Nats(async_nats::Client),
    Ws(WsClient),
}

impl BusClient {
    /// Connect to the bus. Dispatches on URL scheme:
    /// - `nats://` → NATS backend (current production)
    /// - `ws://` or `wss://` → eventbus WS transport (Phase 5)
    ///
    /// `source` is the plugin/service id stamped onto envelopes.
    pub async fn connect(url: &str, source: &str) -> Result<Self> {
        let backend = if url.starts_with("ws://") || url.starts_with("wss://") {
            let ws = WsClient::connect(url, source).await?;
            BackendHandle::Ws(ws)
        } else {
            // Default to NATS for nats:// and any other scheme.
            let client = async_nats::connect(url)
                .await
                .with_context(|| format!("failed to connect to bus at {url}"))?;
            info!(url, source, "connected to bus (NATS backend)");
            BackendHandle::Nats(client)
        };

        Ok(Self {
            backend,
            source: source.to_string(),
        })
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
        match &self.backend {
            BackendHandle::Nats(client) => {
                let subject = subject.as_ref();
                client
                    .publish(subject.to_string(), payload.into())
                    .await
                    .with_context(|| format!("failed to publish to {subject}"))?;
                debug!(subject, "published message");
            }
            BackendHandle::Ws(ws) => {
                let bytes: bytes::Bytes = payload.into();
                ws.publish_bytes(subject, bytes.as_ref()).await?;
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
        match &self.backend {
            BackendHandle::Nats(client) => {
                let subject = subject.as_ref();
                let response = client
                    .request(subject.to_string(), payload.into())
                    .await
                    .with_context(|| format!("bus request to {subject} failed"))?;
                debug!(subject, "received response");
                Ok(response.payload.to_vec())
            }
            BackendHandle::Ws(ws) => {
                let bytes: bytes::Bytes = payload.into();
                ws.request_bytes(subject, bytes.as_ref()).await
            }
        }
    }

    /// Subscribe to a subject pattern. Returns a [`Subscription`]
    /// that yields [`Message`]s via `.next().await`.
    pub async fn subscribe(&self, subject: impl AsRef<str>) -> Result<Subscription> {
        match &self.backend {
            BackendHandle::Nats(client) => {
                let subject = subject.as_ref();
                let mut nats_sub = client
                    .subscribe(subject.to_string())
                    .await
                    .with_context(|| format!("failed to subscribe to {subject}"))?;
                info!(subject, "subscribed");

                // Bridge: convert async_nats::Message → WsMessage on
                // a background task, feeding into an mpsc that the
                // returned WsSubscription reads from.
                let (tx, rx) = tokio::sync::mpsc::channel::<WsMessage>(256);
                let sub_id = ulid::Ulid::new().to_string();
                tokio::spawn(async move {
                    use futures_util::StreamExt;
                    while let Some(msg) = nats_sub.next().await {
                        let ws_msg = WsMessage {
                            subject: msg.subject.to_string(),
                            payload: msg.payload.clone(),
                            reply: msg.reply.map(|r| r.to_string()),
                        };
                        if tx.send(ws_msg).await.is_err() {
                            break;
                        }
                    }
                });

                Ok(WsSubscription { rx, id: sub_id })
            }
            BackendHandle::Ws(ws) => ws.subscribe(subject).await,
        }
    }

    /// Reply directly to a captured reply subject.
    pub async fn reply_to(
        &self,
        reply_subject: &Subject,
        payload: impl Into<bytes::Bytes>,
    ) -> Result<()> {
        self.publish_bytes(reply_subject.as_str(), payload).await
    }

    /// Returns `true` if the underlying bus connection is healthy.
    pub fn is_connected(&self) -> bool {
        match &self.backend {
            BackendHandle::Nats(client) => {
                client.connection_state() == async_nats::connection::State::Connected
            }
            BackendHandle::Ws(ws) => ws.is_connected(),
        }
    }
}
