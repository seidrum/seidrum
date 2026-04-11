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

/// Result of validating a webhook URL — includes the resolved IP for
/// caller-side DNS pinning. Returned by [`validate_webhook_url_resolved`]
/// and used by `WebhookChannel` / `WebhookInterceptor` to dial the
/// validated address directly via `reqwest::Client::resolve`, closing
/// the DNS rebinding window between validation and delivery.
#[derive(Debug, Clone)]
pub struct ValidatedWebhookUrl {
    pub host: String,
    pub port: u16,
    pub resolved_ip: IpAddr,
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
/// Convenience wrapper that discards the resolved IP. Use
/// [`validate_webhook_url_resolved`] when you need the IP for DNS
/// pinning at delivery time.
pub fn validate_webhook_url_with_policy(
    url: &str,
    policy: WebhookUrlPolicy,
) -> Result<(), WebhookUrlError> {
    validate_webhook_url_resolved(url, policy).map(|_| ())
}

/// Validate a webhook URL and return the resolved IP for DNS pinning.
///
/// **Strict policy enforces (C2 — SSRF mitigation):**
/// - URL parses cleanly
/// - Scheme is `https://` (no `http://`, no `file://`, no `gopher://`)
/// - Host is set
/// - Resolved IP(s) are not loopback, private (RFC 1918 / RFC 4193),
///   link-local (169.254.0.0/16, fe80::/10), or unspecified (0.0.0.0, ::)
///
/// **N7:** literal IPv4/IPv6 hosts are checked directly via
/// `url::Host::Ipv4`/`Ipv6`, not by routing through `getaddrinfo`. This
/// removes the dependency on libc's bracketed-IPv6 handling and makes
/// the validator deterministic across platforms.
///
/// **N1:** returns the resolved IP so callers can dial that exact
/// address (via `reqwest::Client::resolve(host, ip)`), closing the
/// DNS rebinding window between validation and delivery.
///
/// **Permissive policy** still parses the URL and rejects unspecified
/// addresses (0.0.0.0, ::), but allows http://, loopback, and private
/// ranges. Use this in tests or in deployments where the bus only
/// dials sibling services on a trusted network.
pub fn validate_webhook_url_resolved(
    url: &str,
    policy: WebhookUrlPolicy,
) -> Result<ValidatedWebhookUrl, WebhookUrlError> {
    let parsed = url::Url::parse(url).map_err(|e| WebhookUrlError::Parse(e.to_string()))?;

    if matches!(policy, WebhookUrlPolicy::Strict) && parsed.scheme() != "https" {
        return Err(WebhookUrlError::InsecureScheme(parsed.scheme().to_string()));
    }
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(WebhookUrlError::InsecureScheme(parsed.scheme().to_string()));
    }

    let host_obj = parsed.host().ok_or(WebhookUrlError::NoHost)?;
    let host_str = parsed
        .host_str()
        .ok_or(WebhookUrlError::NoHost)?
        .to_string();
    let port = parsed.port_or_known_default().unwrap_or(443);

    // N7: handle literal IPs directly so we don't depend on getaddrinfo
    // semantics for bracketed/embedded forms. For DNS hostnames, fall
    // through to the resolver (which still triggers a synchronous
    // lookup but operates only on real hostnames).
    let resolved_ip: IpAddr = match host_obj {
        url::Host::Ipv4(v4) => IpAddr::V4(v4),
        url::Host::Ipv6(v6) => IpAddr::V6(v6),
        url::Host::Domain(domain) => {
            use std::net::ToSocketAddrs;
            let mut addrs = (domain, port)
                .to_socket_addrs()
                .map_err(|e| WebhookUrlError::DnsError(domain.to_string(), e.to_string()))?;
            // Take the first resolved address but check ALL of them
            // against the policy below. We pin to the first one for
            // delivery; if any address fails policy we reject the URL
            // entirely (so an attacker can't use round-robin DNS to
            // smuggle a private IP behind a public-IP-first response).
            let first = addrs
                .next()
                .ok_or_else(|| {
                    WebhookUrlError::DnsError(domain.to_string(), "no address resolved".to_string())
                })?
                .ip();
            // Re-resolve and check every address.
            for sa in (domain, port)
                .to_socket_addrs()
                .map_err(|e| WebhookUrlError::DnsError(domain.to_string(), e.to_string()))?
            {
                check_address_against_policy(&host_str, sa.ip(), policy)?;
            }
            first
        }
    };
    check_address_against_policy(&host_str, resolved_ip, policy)?;

    Ok(ValidatedWebhookUrl {
        host: host_str,
        port,
        resolved_ip,
    })
}

fn check_address_against_policy(
    host: &str,
    ip: IpAddr,
    policy: WebhookUrlPolicy,
) -> Result<(), WebhookUrlError> {
    match policy {
        WebhookUrlPolicy::Strict => {
            if is_private_ip(&ip) {
                return Err(WebhookUrlError::PrivateAddress(host.to_string(), ip));
            }
        }
        WebhookUrlPolicy::Permissive => {
            // Even Permissive rejects 0.0.0.0 / :: — those are never
            // a meaningful destination address.
            if ip.is_unspecified() {
                return Err(WebhookUrlError::PrivateAddress(host.to_string(), ip));
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
/// - **N1 (DNS rebinding):** every `deliver()` call re-runs the URL
///   through the configured policy. If DNS now resolves the host to a
///   private/loopback address, the delivery fails immediately. The
///   per-call DNS lookup is negligible compared to the TLS handshake
///   that follows.
pub struct WebhookChannel {
    client: Client,
    /// SSRF policy applied on every `deliver()` call. Defaults to
    /// `Strict` so production deployments are safe by default; the
    /// builder threads the configured policy through here.
    policy: WebhookUrlPolicy,
}

impl WebhookChannel {
    /// Create a new webhook delivery channel with the strict SSRF policy.
    pub fn new() -> Arc<Self> {
        Self::with_policy(WebhookUrlPolicy::Strict)
    }

    /// Create a new webhook delivery channel with an explicit SSRF policy.
    /// Use [`WebhookUrlPolicy::Permissive`] only for tests / trusted networks.
    pub fn with_policy(policy: WebhookUrlPolicy) -> Arc<Self> {
        let client = Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client builder should succeed with default settings");
        Arc::new(Self { client, policy })
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

        // N1 + F5 + F6: validate-and-pin the URL on every delivery.
        // The validator runs synchronous DNS via to_socket_addrs, so we
        // wrap it in spawn_blocking to keep the tokio worker free
        // (F6). The returned ValidatedWebhookUrl carries the resolved
        // IP, which we then pass to reqwest::Client::resolve so the
        // dial uses the validated address rather than re-resolving
        // (closing the prior validator-vs-reqwest TOCTOU — F5).
        let policy = self.policy;
        let url_for_blocking = url.clone();
        let validated = match tokio::task::spawn_blocking(move || {
            validate_webhook_url_resolved(&url_for_blocking, policy)
        })
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                warn!(
                    url = %url,
                    error = %e,
                    "WebhookChannel rejecting delivery — URL no longer passes policy (DNS rebinding?)"
                );
                return Err(DeliveryError::Permanent(format!(
                    "URL no longer passes SSRF policy: {}",
                    e
                )));
            }
            Err(join_err) => {
                return Err(DeliveryError::Failed(format!(
                    "validation task panicked: {}",
                    join_err
                )));
            }
        };

        // Build a per-call client pinned to the validated IP. This is
        // wasteful for a hot path (each delivery allocates a new
        // connection pool) but it is the simplest way to close the
        // dual-DNS-lookup window. A future optimisation can cache
        // pinned clients keyed by (host, ip).
        let pinned_addr = std::net::SocketAddr::new(validated.resolved_ip, validated.port);
        let pinned_client = match Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(30))
            .resolve(&validated.host, pinned_addr)
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                return Err(DeliveryError::Failed(format!(
                    "failed to build pinned client: {}",
                    e
                )));
            }
        };

        // Build the event payload
        let payload_b64 = base64::engine::general_purpose::STANDARD.encode(event);
        let body = json!({
            "subject": subject,
            "payload": payload_b64,
        });

        // Build the request via the pinned client.
        let mut req = pinned_client.post(&url);

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

    // === N8a / C2 SSRF unit tests ===
    //
    // These exercise validate_webhook_url(_with_policy) directly so a
    // regression in the SSRF allowlist is caught before any e2e test runs.

    #[test]
    fn test_ssrf_https_public_accepted() {
        // example.com (RFC 2606) resolves to a public IP allocated to IANA.
        // If DNS is unavailable, this test is skipped via the DnsError branch.
        let r = validate_webhook_url("https://example.com/hook");
        // Either pass (DNS available, IP is public) or fail with DnsError
        // (offline build), but never PrivateAddress.
        match r {
            Ok(_) => {}
            Err(WebhookUrlError::DnsError(_, _)) => {}
            Err(e) => panic!("unexpected error for example.com: {}", e),
        }
    }

    #[test]
    fn test_ssrf_http_rejected_by_strict() {
        let r = validate_webhook_url("http://example.com/hook");
        assert!(matches!(r, Err(WebhookUrlError::InsecureScheme(_))));
    }

    #[test]
    fn test_ssrf_loopback_v4_rejected() {
        let r = validate_webhook_url("https://127.0.0.1/hook");
        assert!(matches!(r, Err(WebhookUrlError::PrivateAddress(_, _))));
    }

    #[test]
    fn test_ssrf_loopback_v6_rejected() {
        let r = validate_webhook_url("https://[::1]/hook");
        assert!(matches!(r, Err(WebhookUrlError::PrivateAddress(_, _))));
    }

    #[test]
    fn test_ssrf_ipv4_mapped_v6_rejected() {
        let r = validate_webhook_url("https://[::ffff:127.0.0.1]/hook");
        assert!(matches!(r, Err(WebhookUrlError::PrivateAddress(_, _))));
    }

    #[test]
    fn test_ssrf_aws_metadata_rejected() {
        let r = validate_webhook_url("https://169.254.169.254/latest/meta-data/");
        assert!(
            matches!(r, Err(WebhookUrlError::PrivateAddress(_, _))),
            "AWS metadata IP must be rejected, got {:?}",
            r
        );
    }

    #[test]
    fn test_ssrf_rfc1918_ranges_rejected() {
        for url in [
            "https://10.0.0.1/",
            "https://172.16.0.1/",
            "https://192.168.1.1/",
        ] {
            let r = validate_webhook_url(url);
            assert!(
                matches!(r, Err(WebhookUrlError::PrivateAddress(_, _))),
                "{} must be rejected, got {:?}",
                url,
                r
            );
        }
    }

    #[test]
    fn test_ssrf_unspecified_rejected() {
        let r = validate_webhook_url("https://0.0.0.0/");
        assert!(matches!(r, Err(WebhookUrlError::PrivateAddress(_, _))));
        let r = validate_webhook_url("https://[::]/");
        assert!(matches!(r, Err(WebhookUrlError::PrivateAddress(_, _))));
    }

    #[test]
    fn test_ssrf_link_local_rejected() {
        let r = validate_webhook_url("https://169.254.0.5/");
        assert!(matches!(r, Err(WebhookUrlError::PrivateAddress(_, _))));
        let r = validate_webhook_url("https://[fe80::1]/");
        assert!(matches!(r, Err(WebhookUrlError::PrivateAddress(_, _))));
    }

    #[test]
    fn test_ssrf_unique_local_v6_rejected() {
        let r = validate_webhook_url("https://[fc00::1]/");
        assert!(matches!(r, Err(WebhookUrlError::PrivateAddress(_, _))));
        let r = validate_webhook_url("https://[fd00::1]/");
        assert!(matches!(r, Err(WebhookUrlError::PrivateAddress(_, _))));
    }

    #[test]
    fn test_ssrf_short_form_loopback_rejected() {
        // 127.1 → 127.0.0.1 in classful IPv4 short form. The url crate
        // parses this as a hostname; whatever the resolver returns must
        // be rejected if it's loopback. We accept either PrivateAddress
        // or DnsError (parser may not understand the short form).
        let r = validate_webhook_url("https://127.1/");
        assert!(
            matches!(r, Err(WebhookUrlError::PrivateAddress(_, _)))
                || matches!(r, Err(WebhookUrlError::DnsError(_, _)))
                || matches!(r, Err(WebhookUrlError::Parse(_))),
            "127.1 short-form must NOT be accepted, got {:?}",
            r
        );
    }

    #[test]
    fn test_ssrf_decimal_ipv4_rejected() {
        // 2130706433 == 127.0.0.1 in decimal. Same handling as 127.1.
        let r = validate_webhook_url("https://2130706433/");
        assert!(
            matches!(r, Err(WebhookUrlError::PrivateAddress(_, _)))
                || matches!(r, Err(WebhookUrlError::DnsError(_, _)))
                || matches!(r, Err(WebhookUrlError::Parse(_))),
            "decimal-encoded loopback must NOT be accepted, got {:?}",
            r
        );
    }

    #[test]
    fn test_ssrf_userinfo_attack() {
        // https://attacker.com@127.0.0.1/ — the URL parser sets host
        // to 127.0.0.1 and `attacker.com` becomes the userinfo. Must be
        // rejected as PrivateAddress.
        let r = validate_webhook_url("https://attacker.com@127.0.0.1/");
        assert!(matches!(r, Err(WebhookUrlError::PrivateAddress(_, _))));
    }

    #[test]
    fn test_ssrf_invalid_url_rejected() {
        let r = validate_webhook_url("not a url");
        assert!(matches!(r, Err(WebhookUrlError::Parse(_))));
    }

    #[test]
    fn test_ssrf_file_scheme_rejected() {
        let r = validate_webhook_url("file:///etc/passwd");
        assert!(matches!(r, Err(WebhookUrlError::InsecureScheme(_))));
    }

    #[test]
    fn test_ssrf_permissive_accepts_loopback_but_rejects_unspecified() {
        let r =
            validate_webhook_url_with_policy("http://127.0.0.1/hook", WebhookUrlPolicy::Permissive);
        assert!(r.is_ok(), "Permissive should accept loopback, got {:?}", r);

        let r =
            validate_webhook_url_with_policy("http://0.0.0.0/hook", WebhookUrlPolicy::Permissive);
        assert!(
            matches!(r, Err(WebhookUrlError::PrivateAddress(_, _))),
            "Permissive must still reject 0.0.0.0, got {:?}",
            r
        );
    }

    #[test]
    fn test_ssrf_permissive_still_rejects_invalid_url() {
        let r = validate_webhook_url_with_policy("not a url", WebhookUrlPolicy::Permissive);
        assert!(matches!(r, Err(WebhookUrlError::Parse(_))));
    }
}
