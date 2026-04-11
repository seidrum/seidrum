//! Webhook delivery channel.
//!
//! Delivers events by HTTP POST to a configured webhook URL.
//! Individual delivery attempts are single-shot; retry logic lives in
//! [`super::RetryTask`] (Phase 5 stub).

use super::{DeliveryChannel, DeliveryError, DeliveryReceipt, DeliveryResult};
use crate::delivery::ChannelConfig;
use async_trait::async_trait;
use base64::Engine;
use reqwest::redirect::Policy;
use reqwest::Client;
use serde_json::json;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tracing::{debug, warn};

/// Timeout for webhook health check requests.
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors returned by [`validate_webhook_url`].
#[derive(Debug, Error)]
pub enum WebhookUrlError {
    #[error("invalid URL: {0}")]
    Parse(String),
    #[error("URL scheme must be https (got {0})")]
    InsecureScheme(String),
    #[error("URL must have a host")]
    NoHost,
    #[error("URL host {0} resolves to a private/loopback/link-local address ({1})")]
    PrivateAddress(String, IpAddr),
    #[error("URL host {0} could not be resolved: {1}")]
    DnsError(String, String),
}

/// Policy controlling how strictly webhook URLs are validated.
///
/// `Strict` (default) enforces the production C2 SSRF mitigation:
/// `https://`-only and non-private hosts. `Permissive` allows `http://`
/// and any host — intended for tests and trusted-network deployments
/// (e.g. an in-cluster bus where every webhook target is a sibling pod).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WebhookUrlPolicy {
    /// Production default: https-only, no private/loopback/link-local hosts.
    #[default]
    Strict,
    /// Allow http:// and private/loopback/link-local hosts. **Use only
    /// in tests or trusted networks.** Still rejects malformed URLs and
    /// unspecified addresses (0.0.0.0).
    Permissive,
}

/// Validate that a webhook URL is safe to dial from the server using the
/// strict (production) policy. Equivalent to
/// `validate_webhook_url_with_policy(url, WebhookUrlPolicy::Strict)`.
pub fn validate_webhook_url(url: &str) -> Result<(), WebhookUrlError> {
    validate_webhook_url_with_policy(url, WebhookUrlPolicy::Strict)
}

/// Validate a webhook URL against the given policy.
///
/// **Strict policy enforces (C2 — SSRF mitigation):**
/// - URL parses cleanly
/// - Scheme is `https://` (no `http://`, no `file://`, no `gopher://`)
/// - Host is set
/// - Resolved IP(s) are not loopback, private (RFC 1918 / RFC 4193),
///   link-local (169.254.0.0/16, fe80::/10), or unspecified (0.0.0.0, ::)
///
/// **What strict policy does NOT enforce:**
/// - DNS rebinding (the resolved IP at validation time may differ from
///   the one used at delivery time). The webhook channel itself disables
///   redirects via `Policy::none()`, which closes one rebinding vector.
/// - TLS certificate pinning
/// - Allowlisting specific domains
///
/// **Permissive policy** still parses the URL and rejects unspecified
/// addresses (0.0.0.0, ::), but allows http://, loopback, and private
/// ranges. Use this in tests or in deployments where the bus only
/// dials sibling services on a trusted network.
pub fn validate_webhook_url_with_policy(
    url: &str,
    policy: WebhookUrlPolicy,
) -> Result<(), WebhookUrlError> {
    let parsed = url::Url::parse(url).map_err(|e| WebhookUrlError::Parse(e.to_string()))?;

    if matches!(policy, WebhookUrlPolicy::Strict) && parsed.scheme() != "https" {
        return Err(WebhookUrlError::InsecureScheme(parsed.scheme().to_string()));
    }
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(WebhookUrlError::InsecureScheme(parsed.scheme().to_string()));
    }

    let host = parsed.host_str().ok_or(WebhookUrlError::NoHost)?.to_string();

    // Resolve the host to one or more IPs and check each one. Using
    // `to_socket_addrs` triggers a synchronous DNS lookup but also handles
    // literal IPv4/IPv6 strings correctly.
    use std::net::ToSocketAddrs;
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addrs = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| WebhookUrlError::DnsError(host.clone(), e.to_string()))?;

    for sa in addrs {
        let ip = sa.ip();
        match policy {
            WebhookUrlPolicy::Strict => {
                if is_private_ip(&ip) {
                    return Err(WebhookUrlError::PrivateAddress(host, ip));
                }
            }
            WebhookUrlPolicy::Permissive => {
                // Even Permissive rejects 0.0.0.0 / :: — those are never
                // a meaningful destination address.
                if ip.is_unspecified() {
                    return Err(WebhookUrlError::PrivateAddress(host, ip));
                }
            }
        }
    }

    Ok(())
}

/// Returns `true` if the given IP is unsafe for webhook delivery
/// (loopback, private, link-local, multicast, or unspecified).
fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                // RFC 6598 / shared address space
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // unique-local fc00::/7
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // link-local fe80::/10
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // IPv4-mapped → check the embedded v4 address
                || v6.to_ipv4_mapped().is_some_and(|v4| is_private_ip(&IpAddr::V4(v4)))
        }
    }
}

/// Webhook delivery channel.
/// Sends events as HTTP POST requests to a configured URL.
///
/// Individual delivery attempts are single-shot. Retry logic is handled
/// externally by [`super::RetryTask`] which polls the event store for
/// failed deliveries and re-invokes the channel.
///
/// **SSRF mitigations:**
/// - The reqwest client is configured with `redirect(Policy::none())` so
///   a server can't 302 the delivery to a private address after the
///   initial URL passed validation.
/// - URL validation happens at subscription time via [`validate_webhook_url`].
pub struct WebhookChannel {
    client: Client,
}

impl WebhookChannel {
    /// Create a new webhook delivery channel.
    pub fn new() -> Arc<Self> {
        let client = Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client builder should succeed with default settings");
        Arc::new(Self { client })
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
            // Treat 408 (Request Timeout), 425 (Too Early — RFC 8470:
            // "try again later"), and 429 (Too Many Requests) as transient.
            // All other 4xx are permanent.
            let code = status.as_u16();
            if status.is_client_error() && code != 408 && code != 425 && code != 429 {
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
