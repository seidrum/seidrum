//! Webhook-based sync interceptor (C3 — PLAN.md L154).
//!
//! Mirrors [`crate::delivery::WsRemoteInterceptor`] but over plain HTTP.
//! When the bus dispatches an event matching the registered pattern,
//! [`WebhookInterceptor::intercept`] POSTs the event to the configured
//! webhook URL and parses the response body as a JSON `intercept_result`
//! document. The action drives the in-process dispatch chain.
//!
//! # Wire format
//!
//! Request body (JSON):
//! ```json
//! {
//!   "subject": "events.example",
//!   "payload": "<base64-encoded payload>"
//! }
//! ```
//!
//! Response body (JSON, identical to the WS `InterceptResult` shape):
//! ```json
//! {"action": "pass"}                        // continue unchanged
//! {"action": "drop"}                        // abort dispatch
//! {"action": "modify", "payload": "<b64>"}  // replace payload
//! ```
//!
//! HTTP status codes:
//! - 2xx → response body is parsed as above
//! - non-2xx → treated as `Pass` so a misbehaving handler does not stall
//!   the dispatch chain (matches the WS proxy's timeout fallback)
//!
//! # Differences from `WsRemoteInterceptor`
//!
//! - **Statelessness:** each `intercept()` call is a fresh HTTP request.
//!   There's no per-connection `pending_replies` map or disconnect
//!   cleanup.
//! - **Latency:** every event pays a full HTTP round-trip. Use this only
//!   for low-volume control-plane subjects.
//! - **No streaming:** unlike WS, there's no way for the remote to
//!   register once and then drain a stream of intercept calls — each
//!   call is a separate request.
//!
//! # SSRF
//!
//! The webhook URL is validated at registration time via
//! [`crate::delivery::validate_webhook_url`] (or the configured
//! [`crate::delivery::WebhookUrlPolicy`]). The interceptor itself uses a
//! `reqwest::Client` configured with `redirect(Policy::none())` so a
//! server can't 302 the request to a private address.

use crate::delivery::{validate_webhook_url_with_policy, WebhookUrlPolicy};
use crate::dispatch::{InterceptResult, Interceptor};
use async_trait::async_trait;
use base64::Engine;
use reqwest::redirect::Policy;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tracing::warn;

/// A sync interceptor that delegates to a remote HTTP webhook.
///
/// See module docs for the wire format.
pub struct WebhookInterceptor {
    pattern: String,
    url: String,
    headers: HashMap<String, String>,
    client: Client,
    /// Per-call timeout. The bus's interceptor timeout is also enforced
    /// independently by the dispatch engine.
    intercept_timeout: Duration,
    /// SSRF policy applied on every `intercept()` call to mitigate DNS
    /// rebinding (N1). Defaults to Strict.
    policy: WebhookUrlPolicy,
}

/// Wire-level intercept result returned by a webhook interceptor.
///
/// Tagged on the `action` field so serde enforces the modify-implies-payload
/// invariant at parse time.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
enum WebhookInterceptResult {
    Pass,
    Drop,
    Modify { payload: String },
}

impl WebhookInterceptor {
    /// Create a new webhook interceptor pointed at `url` with the
    /// strict SSRF policy.
    pub fn new(pattern: String, url: String, headers: HashMap<String, String>) -> Self {
        Self::with_policy(pattern, url, headers, WebhookUrlPolicy::Strict)
    }

    /// Create a new webhook interceptor with an explicit SSRF policy.
    /// Tests use `Permissive` to allow loopback URLs.
    pub fn with_policy(
        pattern: String,
        url: String,
        headers: HashMap<String, String>,
        policy: WebhookUrlPolicy,
    ) -> Self {
        let client = Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client builder should succeed with default settings");
        Self {
            pattern,
            url,
            headers,
            client,
            intercept_timeout: Duration::from_secs(5),
            policy,
        }
    }

    /// Override the per-call timeout. Default 5 seconds.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.intercept_timeout = timeout;
        // Rebuild the client with the new timeout so it actually applies
        // at the HTTP layer (not just at the tokio::time::timeout layer).
        self.client = Client::builder()
            .redirect(Policy::none())
            .timeout(timeout)
            .build()
            .expect("reqwest client builder should succeed");
        self
    }

    /// The URL this interceptor delivers to. Used for logging.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The pattern this interceptor was registered against.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }
}

#[async_trait]
impl Interceptor for WebhookInterceptor {
    async fn intercept(&self, subject: &str, payload: &mut Vec<u8>) -> InterceptResult {
        // N1: re-validate the URL against the policy on every call so a
        // DNS rebinding attack (host that resolved to a public IP at
        // registration now resolves to 127.0.0.1) returns Pass instead
        // of forwarding the (potentially-secret) payload to an internal
        // address. Returning Pass keeps dispatch flowing.
        if let Err(e) = validate_webhook_url_with_policy(&self.url, self.policy) {
            warn!(
                pattern = %self.pattern,
                url = %self.url,
                error = %e,
                "WebhookInterceptor URL no longer passes policy (DNS rebinding?); passing through"
            );
            return InterceptResult::Pass;
        }

        let payload_b64 = base64::engine::general_purpose::STANDARD.encode(payload.as_slice());
        let body = serde_json::json!({
            "subject": subject,
            "payload": payload_b64,
        });

        let mut req = self.client.post(&self.url);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }

        let response = match req.json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    pattern = %self.pattern,
                    url = %self.url,
                    error = %e,
                    "WebhookInterceptor request failed; passing event through"
                );
                return InterceptResult::Pass;
            }
        };

        if !response.status().is_success() {
            warn!(
                pattern = %self.pattern,
                url = %self.url,
                status = %response.status(),
                "WebhookInterceptor non-2xx response; passing event through"
            );
            return InterceptResult::Pass;
        }

        let parsed: WebhookInterceptResult = match response.json().await {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    pattern = %self.pattern,
                    url = %self.url,
                    error = %e,
                    "WebhookInterceptor response not valid JSON; passing event through"
                );
                return InterceptResult::Pass;
            }
        };

        match parsed {
            WebhookInterceptResult::Pass => InterceptResult::Pass,
            WebhookInterceptResult::Drop => InterceptResult::Drop,
            WebhookInterceptResult::Modify { payload: p } => {
                match base64::engine::general_purpose::STANDARD.decode(&p) {
                    Ok(bytes) => {
                        *payload = bytes;
                        InterceptResult::Modified
                    }
                    Err(e) => {
                        warn!(
                            pattern = %self.pattern,
                            url = %self.url,
                            error = %e,
                            "WebhookInterceptor modify payload not valid base64; passing through"
                        );
                        InterceptResult::Pass
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Verify the response wire format parses correctly for each action.
    /// **N9 fix:** previous version of these tests used bare `matches!`
    /// without `assert!`, so they passed regardless of the variant.
    #[test]
    fn test_parse_pass() {
        let v: WebhookInterceptResult = serde_json::from_value(json!({"action": "pass"})).unwrap();
        assert!(matches!(v, WebhookInterceptResult::Pass));
    }

    #[test]
    fn test_parse_drop() {
        let v: WebhookInterceptResult = serde_json::from_value(json!({"action": "drop"})).unwrap();
        assert!(matches!(v, WebhookInterceptResult::Drop));
    }

    #[test]
    fn test_parse_modify() {
        let v: WebhookInterceptResult =
            serde_json::from_value(json!({"action": "modify", "payload": "aGVsbG8="})).unwrap();
        match v {
            WebhookInterceptResult::Modify { payload } => assert_eq!(payload, "aGVsbG8="),
            _ => panic!("expected Modify"),
        }
    }

    #[test]
    fn test_parse_modify_missing_payload_rejected() {
        // serde should reject this at parse time because `payload` is
        // a required field of the `Modify` variant.
        let result: Result<WebhookInterceptResult, _> =
            serde_json::from_value(json!({"action": "modify"}));
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_unknown_action_rejected() {
        let result: Result<WebhookInterceptResult, _> =
            serde_json::from_value(json!({"action": "explode"}));
        assert!(result.is_err());
    }
}
