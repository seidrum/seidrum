// NATS connection, publish, and subscribe helpers.

use anyhow::{Context, Result};
use async_nats::Subscriber;
use serde::{de::DeserializeOwned, Serialize};
use tracing::{debug, info};

use crate::events::EventEnvelope;

/// Thin wrapper around `async_nats::Client` with convenience methods.
#[derive(Clone)]
pub struct NatsClient {
    client: async_nats::Client,
    /// Default source identifier used when wrapping payloads in envelopes.
    pub source: String,
}

impl NatsClient {
    /// Connect to a NATS server.
    pub async fn connect(url: &str, source: &str) -> Result<Self> {
        let client = async_nats::connect(url)
            .await
            .with_context(|| format!("failed to connect to NATS at {url}"))?;
        info!(url, source, "connected to NATS");
        Ok(Self {
            client,
            source: source.to_string(),
        })
    }

    /// Publish a serializable payload to a NATS subject.
    pub async fn publish<T: Serialize>(&self, subject: &str, payload: &T) -> Result<()> {
        let bytes = serde_json::to_vec(payload)
            .with_context(|| format!("failed to serialize payload for {subject}"))?;
        self.client
            .publish(subject.to_string(), bytes.into())
            .await
            .with_context(|| format!("failed to publish to {subject}"))?;
        debug!(subject, "published message");
        Ok(())
    }

    /// Publish a payload wrapped in an `EventEnvelope`.
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

    /// Send a NATS request (request/reply) and deserialize the response.
    pub async fn request<T: Serialize, R: DeserializeOwned>(
        &self,
        subject: &str,
        payload: &T,
    ) -> Result<R> {
        let bytes = serde_json::to_vec(payload)
            .with_context(|| format!("failed to serialize request for {subject}"))?;
        let response = self
            .client
            .request(subject.to_string(), bytes.into())
            .await
            .with_context(|| format!("NATS request to {subject} failed"))?;
        let result: R = serde_json::from_slice(&response.payload)
            .with_context(|| format!("failed to deserialize response from {subject}"))?;
        debug!(subject, "received response");
        Ok(result)
    }

    /// Subscribe to a NATS subject.
    pub async fn subscribe(&self, subject: &str) -> Result<Subscriber> {
        let subscriber = self
            .client
            .subscribe(subject.to_string())
            .await
            .with_context(|| format!("failed to subscribe to {subject}"))?;
        info!(subject, "subscribed");
        Ok(subscriber)
    }

    /// Return a reference to the underlying `async_nats::Client`.
    pub fn inner(&self) -> &async_nats::Client {
        &self.client
    }
}
