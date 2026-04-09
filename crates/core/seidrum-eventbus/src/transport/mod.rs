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
}
