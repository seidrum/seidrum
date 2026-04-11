//! Delivery channels for forwarding events to subscribers.
//!
//! The dispatch engine routes in-process events through mpsc channels
//! returned from [`crate::EventBus::subscribe`]. The types in this module
//! provide additional delivery targets used by the transport layer:
//!
//! - [`webhook::WebhookChannel`] — POST events to an HTTP webhook URL.
//!   Used by the HTTP transport's `/subscribe` handler.
//! - [`websocket::WebSocketChannel`] — push events as JSON frames to an
//!   externally-managed WebSocket connection.
//! - [`in_process::InProcessChannel`] — standalone in-process channel for
//!   embedding the [`DeliveryChannel`] trait in custom pipelines.
//! - [`registry::ChannelRegistry`] — runtime registry of custom channel
//!   implementations keyed by type name.
//! - [`retry::RetryTask`] — background task that polls the store for
//!   retryable failed deliveries (Phase 5 stub).

pub mod in_process;
pub mod registry;
pub mod retry;
pub mod webhook;
pub mod webhook_interceptor;
pub mod websocket;
pub mod ws_remote;
pub mod ws_remote_interceptor;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

pub use in_process::InProcessChannel;
pub use registry::ChannelRegistry;
pub use retry::{calculate_backoff, next_retry_after, RetryConfig, RetryTask};
pub use webhook::{
    validate_webhook_url, validate_webhook_url_resolved, validate_webhook_url_with_policy,
    ValidatedWebhookUrl, WebhookChannel, WebhookUrlError, WebhookUrlPolicy,
};
pub use webhook_interceptor::WebhookInterceptor;
pub use websocket::{WebSocketChannel, WebSocketMessage};
pub use ws_remote::{WsDeliveryReply, WsOutboundFrame, WsRemoteChannel};
pub use ws_remote_interceptor::{WsInterceptAction, WsInterceptReply, WsRemoteInterceptor};

#[derive(Debug, Error)]
pub enum DeliveryError {
    /// Transient delivery failure — the delivery should be retried.
    #[error("delivery failed: {0}")]
    Failed(String),
    /// Permanent delivery failure — retrying will not help. The retry task
    /// dead-letters deliveries that return this variant.
    #[error("delivery permanently failed: {0}")]
    Permanent(String),
    /// The channel is not ready to deliver (e.g., closed connection).
    #[error("channel not ready")]
    NotReady,
}

pub type DeliveryResult<T> = Result<T, DeliveryError>;

/// Receipt of successful delivery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryReceipt {
    pub delivered_at: u64, // unix milliseconds
    pub latency_us: u64,
}

/// Configuration for a delivery channel.
///
/// `ChannelConfig` is metadata stored on each subscription. It is read by
/// transport-layer code (e.g., the HTTP webhook delivery task uses
/// `ChannelConfig::Webhook` to know where to POST events). The dispatch
/// engine itself does not act on this field — delivery routing for
/// in-process subscribers always uses the mpsc channel returned by
/// `subscribe()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChannelConfig {
    /// In-process delivery via mpsc channel.
    InProcess,
    /// WebSocket subscription tag. The WS transport server forwards events
    /// through its own per-connection mpsc channel; this variant exists
    /// purely as metadata so callers can identify WS-backed subscriptions
    /// when inspecting [`crate::SubscribeOpts`].
    WebSocket,
    /// HTTP webhook target. Used by the HTTP transport's delivery task
    /// to POST events to the configured URL.
    Webhook {
        url: String,
        headers: HashMap<String, String>,
    },
    /// User-defined channel type, looked up via [`ChannelRegistry`].
    Custom {
        channel_type: String,
        config: serde_json::Value,
    },
}

/// A delivery channel is responsible for actually delivering an event to a subscriber.
#[async_trait]
pub trait DeliveryChannel: Send + Sync + 'static {
    /// Deliver one event to the subscriber. Returns a receipt on success
    /// or an error that triggers retry.
    async fn deliver(
        &self,
        event: &[u8],
        subject: &str,
        config: &ChannelConfig,
    ) -> DeliveryResult<DeliveryReceipt>;

    /// Called when the subscription is removed or the subscriber disconnects.
    async fn cleanup(&self, config: &ChannelConfig) -> DeliveryResult<()>;

    /// Health check. Returns true if the channel is operational.
    async fn is_healthy(&self, config: &ChannelConfig) -> bool;
}
