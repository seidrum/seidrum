//! API key and JWT authentication.

use crate::jwt::JwtService;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Authentication result containing validated claims.
#[derive(Debug, Clone)]
pub struct AuthResult {
    pub subject: String,
    pub role: String,
    pub scopes: Vec<String>,
}

impl AuthResult {
    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }
}

/// Validate a provided API key against the expected key.
pub fn validate_api_key(provided: &str, expected: &str) -> bool {
    // Simple constant-length comparison to avoid trivial timing leaks.
    // For a personal platform this is sufficient; use `subtle` crate for
    // production-grade constant-time comparison.
    if provided.len() != expected.len() {
        return false;
    }
    let mut result = 0u8;
    for (a, b) in provided.bytes().zip(expected.bytes()) {
        result |= a ^ b;
    }
    result == 0
}

/// Authentication handler that supports both API key and JWT Bearer tokens.
#[derive(Clone)]
pub struct AuthHandler {
    pub jwt_service: Option<JwtService>,
    api_key: String,
}

impl AuthHandler {
    pub fn new(api_key: String, jwt_secret: Option<String>, jwt_ttl: u64) -> Self {
        let jwt_service = jwt_secret.map(|secret| JwtService::new(&secret, jwt_ttl));
        Self {
            jwt_service,
            api_key,
        }
    }

    /// Authenticate a request, supporting both API key and JWT Bearer token.
    /// Returns AuthResult if valid, None otherwise.
    pub fn authenticate(&self, auth_header: &str, api_key_param: Option<String>) -> Option<AuthResult> {
        // Try JWT Bearer token first
        if let Some(token) = auth_header.strip_prefix("Bearer ") {
            if let Some(jwt_service) = &self.jwt_service {
                match jwt_service.validate_token(token) {
                    Ok(claims) => {
                        return Some(AuthResult {
                            subject: claims.sub,
                            role: claims.role,
                            scopes: claims.scopes,
                        });
                    }
                    Err(e) => {
                        warn!("JWT validation failed: {}", e);
                    }
                }
            }
        }

        // Try API key from header or query parameter
        let api_key = api_key_param.unwrap_or_default();
        if !api_key.is_empty() && validate_api_key(&api_key, &self.api_key) {
            return Some(AuthResult {
                subject: "api-key".to_string(),
                role: "admin".to_string(),
                scopes: vec!["scope_root".to_string()],
            });
        }

        // Try X-Api-Key header
        if let Some(header_key) = auth_header.strip_prefix("ApiKey ") {
            if validate_api_key(header_key, &self.api_key) {
                return Some(AuthResult {
                    subject: "api-key".to_string(),
                    role: "admin".to_string(),
                    scopes: vec!["scope_root".to_string()],
                });
            }
        }

        None
    }
}

/// Request to obtain a JWT token.
#[derive(Deserialize)]
pub struct TokenRequest {
    pub api_key: String,
    pub subject: Option<String>,
    pub role: Option<String>,
    pub scopes: Option<Vec<String>>,
}

/// Response containing the JWT token.
#[derive(Serialize)]
pub struct TokenResponse {
    pub token: String,
    pub expires_in: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_api_key() {
        assert!(validate_api_key("secret123", "secret123"));
    }

    #[test]
    fn invalid_api_key() {
        assert!(!validate_api_key("wrong", "secret123"));
    }

    #[test]
    fn empty_api_key() {
        assert!(!validate_api_key("", "secret123"));
    }

    #[test]
    fn both_empty_api_key() {
        assert!(validate_api_key("", ""));
    }

    #[test]
    fn test_auth_handler_with_api_key() {
        let handler = AuthHandler::new("test-key".to_string(), None, 3600);
        let result = handler.authenticate("", Some("test-key".to_string()));
        assert!(result.is_some());
        let auth = result.unwrap();
        assert_eq!(auth.role, "admin");
    }

    #[test]
    fn test_auth_handler_invalid_api_key() {
        let handler = AuthHandler::new("test-key".to_string(), None, 3600);
        let result = handler.authenticate("", Some("wrong-key".to_string()));
        assert!(result.is_none());
    }

    #[test]
    fn test_auth_handler_with_jwt() {
        let jwt_secret = "test-secret";
        let handler = AuthHandler::new("api-key".to_string(), Some(jwt_secret.to_string()), 3600);

        // Generate a token using the JWT service
        let jwt_service = handler.jwt_service.as_ref().unwrap();
        let token = jwt_service
            .generate_token("user1", "admin", vec!["scope_root".to_string()])
            .unwrap();

        let result = handler.authenticate(&format!("Bearer {}", token), None);
        assert!(result.is_some());
        let auth = result.unwrap();
        assert_eq!(auth.subject, "user1");
        assert_eq!(auth.role, "admin");
    }
}
