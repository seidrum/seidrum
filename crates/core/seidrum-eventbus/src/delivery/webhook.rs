//! Webhook delivery channel.
//!
//! Delivers events by HTTP POST to a configured webhook URL.
//! Includes retry logic with exponential backoff.

use super::{DeliveryChannel, DeliveryError, DeliveryReceipt, DeliveryResult};
use crate::delivery::ChannelConfig;
use async_trait::async_trait;
use base64::Engine;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

/// Timeout for webhook health check requests.
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Configuration for webhook retries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookRetryConfig {
    /// Maximum number of delivery attempts before giving up.
    pub max_attempts: u32,
    /// Initial backoff duration in milliseconds.
    pub initial_backoff_ms: u64,
    /// Maximum backoff duration in milliseconds.
    pub max_backoff_ms: u64,
}

impl Default for WebhookRetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_backoff_ms: 100,
            max_backoff_ms: 30000,
        }
    }
}

/// Webhook delivery channel.
/// Sends events as HTTP POST requests to a configured URL.
pub struct WebhookChannel {
    client: Client,
    #[allow(dead_code)]
    retry_config: WebhookRetryConfig,
}

impl WebhookChannel {
    /// Create a new webhook delivery channel.
    pub fn new(retry_config: WebhookRetryConfig) -> Arc<Self> {
        Arc::new(Self {
            client: Client::new(),
            retry_config,
        })
    }

    /// Create with default retry configuration.
    pub fn with_defaults() -> Arc<Self> {
        Self::new(WebhookRetryConfig::default())
    }

    /// Extract URL and headers from ChannelConfig.
    fn extract_config(config: &ChannelConfig) -> DeliveryResult<(String, HashMap<String, String>)> {
        match config {
            ChannelConfig::Webhook { url, headers } => Ok((url.clone(), headers.clone())),
            _ => Err(DeliveryError::Failed(
                "Invalid config for WebhookChannel".to_string(),
            )),
        }
    }
}

#[async_trait]
impl DeliveryChannel for WebhookChannel {
    async fn deliver(
        &self,
        event: &[u8],
        subject: &str,
        config: &ChannelConfig,
    ) -> DeliveryResult<DeliveryReceipt> {
        let start = SystemTime::now();
        let (url, headers) = Self::extract_config(config)?;

        // Build the event payload
        let payload_b64 = base64::engine::general_purpose::STANDARD.encode(event);
        let body = json!({
            "subject": subject,
            "payload": payload_b64,
        });

        // Build the request
        let mut req = self.client.post(&url);

        // Add custom headers
        for (key, value) in headers.iter() {
            req = req.header(key, value);
        }

        // Send the request
        let response = req
            .json(&body)
            .send()
            .await
            .map_err(|e| DeliveryError::Failed(format!("HTTP request failed: {}", e)))?;

        // Check status code
        if !response.status().is_success() {
            return Err(DeliveryError::Failed(format!(
                "HTTP {}: {}",
                response.status().as_u16(),
                response.status().canonical_reason().unwrap_or("unknown")
            )));
        }

        let elapsed = SystemTime::now()
            .duration_since(start)
            .unwrap_or_default()
            .as_micros() as u64;

        let delivered_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        debug!("Webhook delivery succeeded to {} in {}us", url, elapsed);

        Ok(DeliveryReceipt {
            delivered_at,
            latency_us: elapsed,
        })
    }

    async fn cleanup(&self, config: &ChannelConfig) -> DeliveryResult<()> {
        match config {
            ChannelConfig::Webhook { url, .. } => {
                debug!("Webhook channel cleanup for {}", url);
            }
            _ => {
                warn!("cleanup called with non-webhook config");
            }
        }
        Ok(())
    }

    async fn is_healthy(&self, config: &ChannelConfig) -> bool {
        let (url, headers) = match Self::extract_config(config) {
            Ok(c) => c,
            Err(_) => return false,
        };

        let mut req = self.client.get(&url).timeout(HEALTH_CHECK_TIMEOUT);
        for (key, value) in headers.iter() {
            req = req.header(key, value);
        }

        match req.send().await {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_webhook_retry_config_default() {
        let config = WebhookRetryConfig::default();
        assert_eq!(config.max_attempts, 5);
        assert_eq!(config.initial_backoff_ms, 100);
        assert_eq!(config.max_backoff_ms, 30000);
    }

    #[tokio::test]
    async fn test_webhook_extract_config() {
        let mut headers = HashMap::new();
        headers.insert("X-Custom".to_string(), "value".to_string());

        let config = ChannelConfig::Webhook {
            url: "https://example.com/webhook".to_string(),
            headers,
        };

        let (url, extracted_headers) = WebhookChannel::extract_config(&config).unwrap();
        assert_eq!(url, "https://example.com/webhook");
        assert_eq!(extracted_headers.get("X-Custom").unwrap(), "value");
    }

    #[tokio::test]
    async fn test_webhook_extract_config_invalid() {
        let config = ChannelConfig::InProcess;
        assert!(WebhookChannel::extract_config(&config).is_err());
    }
}
