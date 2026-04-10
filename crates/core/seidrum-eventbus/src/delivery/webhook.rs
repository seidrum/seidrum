//! Webhook delivery channel.
//!
//! Delivers events by HTTP POST to a configured webhook URL.
//! Individual delivery attempts are single-shot; retry logic lives in
//! [`super::RetryTask`] (Phase 5 stub).

use super::{DeliveryChannel, DeliveryError, DeliveryReceipt, DeliveryResult};
use crate::delivery::ChannelConfig;
use async_trait::async_trait;
use base64::Engine;
use reqwest::Client;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

/// Timeout for webhook health check requests.
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Webhook delivery channel.
/// Sends events as HTTP POST requests to a configured URL.
///
/// Individual delivery attempts are single-shot. Retry logic is handled
/// externally by [`super::RetryTask`] which polls the event store for
/// failed deliveries and re-invokes the channel.
pub struct WebhookChannel {
    client: Client,
}

impl WebhookChannel {
    /// Create a new webhook delivery channel.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            client: Client::new(),
        })
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

        // Classify status code: 4xx is permanent (auth errors, not-found,
        // method-not-allowed don't get better with retries); 5xx and other
        // failures are transient.
        let status = response.status();
        if !status.is_success() {
            let msg = format!(
                "HTTP {}: {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or("unknown")
            );
            // Treat 408 Request Timeout and 429 Too Many Requests as transient.
            if status.is_client_error() && status.as_u16() != 408 && status.as_u16() != 429 {
                return Err(DeliveryError::Permanent(msg));
            }
            return Err(DeliveryError::Failed(msg));
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

        // Use HEAD to avoid triggering side-effects on POST-only endpoints.
        // Fall back to treating connection success as healthy if HEAD returns
        // 405 Method Not Allowed.
        let mut req = self.client.head(&url).timeout(HEALTH_CHECK_TIMEOUT);
        for (key, value) in headers.iter() {
            req = req.header(key, value);
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                status.is_success() || status == reqwest::StatusCode::METHOD_NOT_ALLOWED
            }
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn test_webhook_channel_new() {
        let _channel = WebhookChannel::new();
    }
}
