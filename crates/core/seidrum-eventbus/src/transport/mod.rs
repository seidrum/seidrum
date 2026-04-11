//! Transport servers for remote event bus access.
//!
//! Provides WebSocket and HTTP transports that allow remote clients
//! to interact with the event bus.

pub mod http;
pub mod ws;

pub use http::{create_router, AppState, ErrorResponse, HttpAuthenticator, HttpServer, NoHttpAuth};
pub use ws::{AuthRequest, Authenticator, NoAuth, WebSocketServer};

/// Default timeout for request/reply operations (milliseconds).
pub const DEFAULT_TIMEOUT_MS: u64 = 5000;

/// Maximum base64-encoded payload size within a JSON body (1 MiB).
pub const MAX_PAYLOAD_SIZE: usize = 1_048_576;

/// Minimum priority value a remote-registered interceptor may use.
///
/// Lower priorities run first; reserving values below this for
/// in-process, trusted interceptors prevents a remote client (WS or
/// HTTP) from inserting itself ahead of e.g. an audit-log or
/// rate-limit interceptor. Both [`crate::transport::ws`] and
/// [`crate::transport::http`] clamp incoming requests to this floor.
pub const MIN_REMOTE_INTERCEPTOR_PRIORITY: u32 = 100;

/// Maximum per-call timeout (in milliseconds) a remote-registered
/// interceptor may request. Mitigates slowloris attacks where an
/// attacker registers many interceptors pointing at a slow upstream:
/// each event would otherwise stall for up to `timeout_ms × N` per
/// publish. The hard cap stops one slow interceptor from blocking the
/// dispatch chain for more than `MAX_REMOTE_INTERCEPTOR_TIMEOUT_MS`.
pub const MAX_REMOTE_INTERCEPTOR_TIMEOUT_MS: u64 = 2000;

/// Maximum length of a remote-supplied subject pattern. Prevents
/// pathological 2-MiB pattern strings from triggering O(N) work in
/// the validator and the trie. 256 chars is more than enough for any
/// realistic subject hierarchy.
pub const MAX_REMOTE_PATTERN_LENGTH: usize = 256;

/// Errors returned by [`validate_remote_pattern`].
#[derive(Debug, thiserror::Error)]
pub enum RemotePatternError {
    #[error("pattern is empty")]
    Empty,
    #[error("pattern length {0} exceeds maximum {1}")]
    TooLong(usize, usize),
    #[error("pattern '{0}' may not match the catch-all '>' / '*' wildcard at the first token (would match reserved '_reply.*' subjects)")]
    UnboundedPrefix(String),
    #[error("pattern '{0}' targets a reserved internal subject ('_reply' / '_reply.*')")]
    Reserved(String),
    #[error("pattern '{0}' contains leading/trailing whitespace")]
    Whitespace(String),
    #[error("pattern '{0}' contains a NUL byte")]
    NulByte(String),
    #[error("pattern '{0}' contains an invalid token: {1}")]
    InvalidToken(String, String),
}

/// Backwards-compat alias for [`RemotePatternError`]. The previous
/// "interceptor-only" name is preserved so external code that imported
/// the error type continues to compile.
pub type InterceptorPatternError = RemotePatternError;

/// Validate that a subject pattern is safe for a **remote** caller to
/// register against — applies equally to interceptors and subscriptions.
///
/// **Why this exists (B1 + F1):** the prior validator only blocked
/// the literal `>` and any pattern with prefix `_reply.`. It missed
/// `*.>`, `*.*`, and similar wildcards in the first token, which the
/// trie happily matches against `_reply.{ulid}` (the internal
/// request/reply correlation subjects). A remote subscriber or
/// interceptor registered on `*.>` could observe, modify, or drop
/// every reply on the bus. The fix originally landed for the
/// interceptor path (B1); F1 extends it to subscribe paths since
/// `POST /subscribe` and the WS `subscribe` op were equally affected.
///
/// The fix is token-aware: the pattern is split on `.` and the
/// **first token must be a concrete literal** (no `*`, no `>`, and
/// not `_reply`). Subsequent tokens are unrestricted — a remote
/// caller can still subscribe to `events.*` or `events.>`, just not
/// to anything that could match `_reply.X`.
///
/// **Hardening (F8):** also rejects patterns longer than
/// [`MAX_REMOTE_PATTERN_LENGTH`] and any pattern containing a NUL
/// byte (which the engine would reject anyway, but layering checks
/// keeps the failure mode predictable for the caller).
///
/// In-process callers that go through `EventBus::intercept` /
/// `EventBus::subscribe` directly are NOT subject to this check —
/// they're trusted.
pub fn validate_remote_pattern(pattern: &str) -> Result<(), RemotePatternError> {
    if pattern.is_empty() {
        return Err(RemotePatternError::Empty);
    }
    if pattern.len() > MAX_REMOTE_PATTERN_LENGTH {
        return Err(RemotePatternError::TooLong(
            pattern.len(),
            MAX_REMOTE_PATTERN_LENGTH,
        ));
    }
    if pattern.contains('\0') {
        return Err(RemotePatternError::NulByte(pattern.to_string()));
    }
    if pattern != pattern.trim() {
        return Err(RemotePatternError::Whitespace(pattern.to_string()));
    }

    // Token-aware analysis.
    let tokens: Vec<&str> = pattern.split('.').collect();
    let first = tokens[0];

    // Reject empty token (e.g. ".foo" or "foo..bar"). The trie may
    // accept these but a remote caller has no business sending them.
    for tok in &tokens {
        if tok.is_empty() {
            return Err(RemotePatternError::InvalidToken(
                pattern.to_string(),
                "empty token (consecutive or leading dots)".to_string(),
            ));
        }
    }

    // First-token must be a concrete literal: no wildcards, no
    // `_reply`. The wildcard ban is the core fix for B1 — `*` and
    // `>` in the first position would otherwise let the pattern
    // match every subject including `_reply.{ulid}`.
    if first == "*" || first == ">" {
        return Err(RemotePatternError::UnboundedPrefix(pattern.to_string()));
    }
    if first == "_reply" {
        return Err(RemotePatternError::Reserved(pattern.to_string()));
    }
    // Reject anything that starts with `_reply.` even though the
    // first-token check above already covers exactly `_reply` —
    // belt-and-suspenders against future tokenizer changes.
    if pattern.starts_with("_reply.") {
        return Err(RemotePatternError::Reserved(pattern.to_string()));
    }

    Ok(())
}

/// Backwards-compat alias for [`validate_remote_pattern`].
#[deprecated(
    since = "0.2.0",
    note = "use validate_remote_pattern — the pattern validator now applies to subscriptions as well as interceptors"
)]
pub fn validate_remote_interceptor_pattern(pattern: &str) -> Result<(), RemotePatternError> {
    validate_remote_pattern(pattern)
}

/// Clamp a remote-registered interceptor priority to the safe floor.
/// Used by both WS and HTTP register-interceptor handlers.
pub fn clamp_remote_interceptor_priority(priority: u32) -> u32 {
    priority.max(MIN_REMOTE_INTERCEPTOR_PRIORITY)
}

/// Clamp a remote-registered interceptor timeout to the safe ceiling.
/// `None` becomes `None` (use the engine default). Returns the clamped
/// timeout in milliseconds.
pub fn clamp_remote_interceptor_timeout(timeout_ms: Option<u64>) -> Option<u64> {
    timeout_ms.map(|t| t.min(MAX_REMOTE_INTERCEPTOR_TIMEOUT_MS))
}

/// Validate and decode a base64 payload, enforcing size limits.
/// Returns the decoded bytes or an error message string.
pub fn validate_and_decode_payload(payload: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;

    if payload.len() > MAX_PAYLOAD_SIZE {
        return Err(format!(
            "Encoded payload {} bytes exceeds limit of {} bytes",
            payload.len(),
            MAX_PAYLOAD_SIZE
        ));
    }
    base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|e| format!("Base64 decode failed: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_payload_ok() {
        let result = validate_and_decode_payload("aGVsbG8=");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), b"hello");
    }

    #[test]
    fn test_validate_payload_too_large() {
        let huge = "A".repeat(MAX_PAYLOAD_SIZE + 1);
        assert!(validate_and_decode_payload(&huge).is_err());
    }

    #[test]
    fn test_validate_payload_invalid_base64() {
        assert!(validate_and_decode_payload("not-valid!!!").is_err());
    }

    // === B1 / N8b regression tests for the shared interceptor pattern validator ===

    #[test]
    fn test_pattern_concrete_first_token_accepted() {
        // Concrete first-token, anything else after — accepted.
        assert!(validate_remote_pattern("events.audit").is_ok());
        assert!(validate_remote_pattern("events.>").is_ok());
        assert!(validate_remote_pattern("events.*").is_ok());
        assert!(validate_remote_pattern("events.user.*").is_ok());
        assert!(validate_remote_pattern("a").is_ok());
    }

    #[test]
    fn test_pattern_wildcard_first_token_rejected() {
        // *. anything → blocked
        assert!(validate_remote_pattern("*").is_err());
        assert!(validate_remote_pattern("*.>").is_err());
        assert!(validate_remote_pattern("*.*").is_err());
        assert!(validate_remote_pattern("*.foo.bar").is_err());
        // > on its own → blocked
        assert!(validate_remote_pattern(">").is_err());
    }

    #[test]
    fn test_pattern_reply_subjects_rejected() {
        assert!(validate_remote_pattern("_reply").is_err());
        assert!(validate_remote_pattern("_reply.foo").is_err());
        assert!(validate_remote_pattern("_reply.*").is_err());
        assert!(validate_remote_pattern("_reply.>").is_err());
    }

    #[test]
    fn test_pattern_whitespace_rejected() {
        assert!(validate_remote_pattern(" events.foo").is_err());
        assert!(validate_remote_pattern("events.foo ").is_err());
        assert!(validate_remote_pattern("events.foo\n").is_err());
    }

    #[test]
    fn test_pattern_empty_rejected() {
        assert!(validate_remote_pattern("").is_err());
    }

    #[test]
    fn test_pattern_empty_token_rejected() {
        assert!(validate_remote_pattern(".foo").is_err());
        assert!(validate_remote_pattern("foo..bar").is_err());
    }

    #[test]
    fn test_pattern_underscored_non_reply_accepted() {
        // `_reply` is the only reserved literal — other underscore-prefixed
        // tokens (e.g. `_internal.foo`) are fine for remote callers.
        assert!(validate_remote_pattern("_internal.foo").is_ok());
    }

    #[test]
    fn test_pattern_length_capped() {
        // F8: reject pathological pattern lengths to prevent O(N) work
        // at parse time on huge inputs.
        let too_long = format!("a.{}", "x".repeat(MAX_REMOTE_PATTERN_LENGTH));
        assert!(matches!(
            validate_remote_pattern(&too_long),
            Err(RemotePatternError::TooLong(_, _))
        ));
        // Just under the cap is fine.
        let ok = "a".repeat(MAX_REMOTE_PATTERN_LENGTH);
        assert!(validate_remote_pattern(&ok).is_ok());
    }

    #[test]
    fn test_pattern_nul_byte_rejected() {
        // F8: NUL bytes are not whitespace, so a `_reply\0.foo` could
        // sneak past the trim check. Engine validate_subject would
        // reject it later, but layering checks keeps the failure mode
        // predictable.
        assert!(matches!(
            validate_remote_pattern("_reply\0.foo"),
            Err(RemotePatternError::NulByte(_))
        ));
        assert!(matches!(
            validate_remote_pattern("events.\0foo"),
            Err(RemotePatternError::NulByte(_))
        ));
    }

    #[test]
    fn test_priority_clamp() {
        assert_eq!(clamp_remote_interceptor_priority(0), MIN_REMOTE_INTERCEPTOR_PRIORITY);
        assert_eq!(clamp_remote_interceptor_priority(50), MIN_REMOTE_INTERCEPTOR_PRIORITY);
        assert_eq!(clamp_remote_interceptor_priority(100), 100);
        assert_eq!(clamp_remote_interceptor_priority(200), 200);
    }

    #[test]
    fn test_timeout_clamp() {
        assert_eq!(clamp_remote_interceptor_timeout(None), None);
        assert_eq!(clamp_remote_interceptor_timeout(Some(500)), Some(500));
        assert_eq!(
            clamp_remote_interceptor_timeout(Some(MAX_REMOTE_INTERCEPTOR_TIMEOUT_MS)),
            Some(MAX_REMOTE_INTERCEPTOR_TIMEOUT_MS)
        );
        assert_eq!(
            clamp_remote_interceptor_timeout(Some(60_000)),
            Some(MAX_REMOTE_INTERCEPTOR_TIMEOUT_MS)
        );
    }
}
