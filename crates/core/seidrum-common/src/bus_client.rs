//! Backend-agnostic bus client used by every Seidrum plugin and kernel
//! service.
//!
//! `BusClient` is the **only** API plugins should use to talk to the
//! kernel. Today it wraps `async_nats::Client`; in Phase 5 the
//! implementation will be swapped to talk directly to
//! `seidrum-eventbus` (in-process for the kernel, via WebSocket for
//! out-of-process plugins). The public surface stays identical so
//! plugin source code does not change.
//!
//! Re-exports of [`Subscription`], [`Message`], and [`Subject`] are
//! intentionally type aliases. Phase 5 will replace them with
//! bus-agnostic equivalents; until then they alias `async_nats` types
//! so plugins can `use seidrum_common::bus_client::*` and never reach
//! into `async_nats` directly.
//!
//! **Do not** add an `inner() -> &async_nats::Client` escape hatch.
//! That kind of leak is exactly the technical debt this rename was
//! created to remove.

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use tracing::{debug, info};

use crate::events::EventEnvelope;

// === Backend-agnostic re-exports ===
//
// These type aliases let plugins import everything they need from
// `seidrum_common::bus_client::*` without ever touching `async_nats`
// directly. Phase 5 will replace the right-hand side with native
// `seidrum-eventbus` types (or thin wrappers) — the left-hand names
// stay the same so plugin code does not change.

/// Subscription handle returned by [`BusClient::subscribe`]. Plugins
/// call `.next().await` on it to receive messages.
pub type Subscription = async_nats::Subscriber;

/// A message delivered to a subscriber. Plugins read `.subject`,
/// `.payload`, and (for request/reply) `.reply` fields.
pub type Message = async_nats::Message;

/// A subject string with the bus's encoding rules. Plugins generally
/// pass `&str` and let the bus accept conversions, but the type is
/// re-exported here for code that stores subjects (e.g. reply
/// addresses captured from incoming requests).
pub type Subject = async_nats::Subject;

/// Backend-agnostic client for talking to the Seidrum bus.
///
/// Today this is a thin wrapper over `async_nats::Client`. In Phase 5
/// the inner field will be replaced with either an in-process
/// `EventBus` handle (kernel) or a WebSocket client to the kernel
/// (plugin process). Callers see the same API in both eras.
#[derive(Clone)]
pub struct BusClient {
    client: async_nats::Client,
    /// Default source identifier used when wrapping payloads in
    /// envelopes via [`Self::publish_envelope`]. Set at connect time.
    pub source: String,
}

impl BusClient {
    /// Connect to the bus. `url` is the connection string for the
    /// current backend (a `nats://` URL today; will become a
    /// `ws://` URL in Phase 5). `source` is the plugin/service id
    /// stamped onto envelopes published via [`Self::publish_envelope`].
    pub async fn connect(url: &str, source: &str) -> Result<Self> {
        let client = async_nats::connect(url)
            .await
            .with_context(|| format!("failed to connect to bus at {url}"))?;
        info!(url, source, "connected to bus");
        Ok(Self {
            client,
            source: source.to_string(),
        })
    }

    /// Publish a serializable payload to a subject. Serializes via
    /// `serde_json` and sends the resulting bytes.
    pub async fn publish<T: Serialize>(&self, subject: impl AsRef<str>, payload: &T) -> Result<()> {
        let subject = subject.as_ref();
        let bytes = serde_json::to_vec(payload)
            .with_context(|| format!("failed to serialize payload for {subject}"))?;
        self.publish_bytes(subject, bytes).await
    }

    /// Publish a raw byte payload to a subject. Used by callers that
    /// have already serialized their payload (e.g. when constructing
    /// an [`EventEnvelope`] manually).
    ///
    /// `subject` accepts `&str`, `String`, `&String`, or anything else
    /// that implements `AsRef<str>`. `payload` accepts `Vec<u8>`,
    /// `&[u8]`, `bytes::Bytes`, etc. via the standard conversions.
    pub async fn publish_bytes(
        &self,
        subject: impl AsRef<str>,
        payload: impl Into<bytes::Bytes>,
    ) -> Result<()> {
        let subject = subject.as_ref();
        self.client
            .publish(subject.to_string(), payload.into())
            .await
            .with_context(|| format!("failed to publish to {subject}"))?;
        debug!(subject, "published message");
        Ok(())
    }

    /// Publish a payload wrapped in an [`EventEnvelope`].
    /// `correlation_id` and `scope` propagate trace context across
    /// the dispatch graph.
    pub async fn publish_envelope<T: Serialize>(
        &self,
        subject: &str,
        correlation_id: Option<String>,
        scope: Option<String>,
        payload: &T,
    ) -> Result<EventEnvelope> {
        let envelope = EventEnvelope::new(subject, &self.source, correlation_id, scope, payload)
            .with_context(|| "failed to build EventEnvelope")?;
        self.publish(subject, &envelope).await?;
        Ok(envelope)
    }

    /// Send a request and deserialize the typed response. The peer
    /// must reply on the auto-generated reply subject before the
    /// backend's request timeout elapses.
    pub async fn request<T: Serialize, R: DeserializeOwned>(
        &self,
        subject: impl AsRef<str>,
        payload: &T,
    ) -> Result<R> {
        let subject = subject.as_ref();
        let bytes = serde_json::to_vec(payload)
            .with_context(|| format!("failed to serialize request for {subject}"))?;
        let response_bytes = self.request_bytes(subject, bytes).await?;
        let result: R = serde_json::from_slice(&response_bytes)
            .with_context(|| format!("failed to deserialize response from {subject}"))?;
        Ok(result)
    }

    /// Send a raw byte request and return the raw byte response.
    /// Used when the caller wants to handle (de)serialization
    /// itself, e.g. forwarding pre-built JSON to an upstream service.
    pub async fn request_bytes(
        &self,
        subject: impl AsRef<str>,
        payload: impl Into<bytes::Bytes>,
    ) -> Result<Vec<u8>> {
        let subject = subject.as_ref();
        let response = self
            .client
            .request(subject.to_string(), payload.into())
            .await
            .with_context(|| format!("bus request to {subject} failed"))?;
        debug!(subject, "received response");
        Ok(response.payload.to_vec())
    }

    /// Subscribe to a subject pattern. Returns a [`Subscription`]
    /// that yields [`Message`]s via `.next().await`. Wildcards `*`
    /// and `>` work as in NATS — see the bus protocol spec for
    /// details.
    pub async fn subscribe(&self, subject: impl AsRef<str>) -> Result<Subscription> {
        let subject = subject.as_ref();
        let subscriber = self
            .client
            .subscribe(subject.to_string())
            .await
            .with_context(|| format!("failed to subscribe to {subject}"))?;
        info!(subject, "subscribed");
        Ok(subscriber)
    }

    /// Reply directly to a captured reply subject. Used by request
    /// handlers that received a [`Message`] with a non-empty `reply`
    /// field. Sends raw bytes; callers serialize themselves.
    pub async fn reply_to(
        &self,
        reply_subject: &Subject,
        payload: impl Into<bytes::Bytes>,
    ) -> Result<()> {
        self.client
            .publish(reply_subject.clone(), payload.into())
            .await
            .with_context(|| format!("failed to reply to {reply_subject}"))?;
        Ok(())
    }

    /// Returns `true` if the underlying bus connection is healthy.
    /// Used by kernel health endpoints to expose liveness. The
    /// definition of "healthy" depends on the backend; today it
    /// reports the NATS connection state, in Phase 5 it will report
    /// the eventbus dispatch engine's running state.
    pub fn is_connected(&self) -> bool {
        self.client.connection_state() == async_nats::connection::State::Connected
    }
}
