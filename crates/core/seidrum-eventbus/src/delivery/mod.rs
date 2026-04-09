pub mod in_process;
pub mod registry;
pub mod retry;
pub mod webhook;
pub mod websocket;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

pub use in_process::InProcessChannel;
pub use registry::ChannelRegistry;
pub use retry::{calculate_backoff, RetryTask};
pub use webhook::WebhookChannel;
pub use websocket::{WebSocketChannel, WebSocketMessage};

#[derive(Debug, Error)]
pub enum DeliveryError {
    #[error("delivery failed: {0}")]
    Failed(String),
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChannelConfig {
    InProcess,
    WebSocket {
        connection_id: String,
    },
    Webhook {
        url: String,
        headers: HashMap<String, String>,
    },
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
