//! API key and JWT authentication with timing-attack protection.

use crate::jwt::JwtService;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tracing::warn;

/// Authentication result containing validated claims.
#[derive(Debug, Clone)]
pub struct AuthResult {
    pub subject: String,
    pub role: String,
    pub scopes: Vec<String>,
    /// User ID for multi-user support (None for API key auth).
    pub user_id: Option<String>,
}

impl AuthResult {
    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }
}

/// Constant-time API key comparison using the `subtle` crate.
/// Prevents timing attacks by always comparing in constant time.
pub fn validate_api_key(provided: &str, expected: &str) -> bool {
    let provided_bytes = provided.as_bytes();
    let expected_bytes = expected.as_bytes();

    // Length check still leaks length info, but this is acceptable:
    // API keys are fixed-length in practice, and the alternative
    // (padding) adds complexity without meaningful security gain.
    if provided_bytes.len() != expected_bytes.len() {
        return false;
    }

    provided_bytes.ct_eq(expected_bytes).into()
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
    ///
    /// Note: This method is async because JWT validation checks the revocation list.
    pub async fn authenticate(
        &self,
        auth_header: &str,
        api_key_param: Option<String>,
    ) -> Option<AuthResult> {
        // Try JWT Bearer token first
        if let Some(token) = auth_header.strip_prefix("Bearer ") {
            if let Some(jwt_service) = &self.jwt_service {
                match jwt_service.validate_token(token).await {
                    Ok(claims) => {
                        return Some(AuthResult {
                            subject: claims.sub,
                            role: claims.role,
                            scopes: claims.scopes,
                            user_id: claims.user_id,
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
                user_id: None,
            });
        }

        // Try X-Api-Key header
        if let Some(header_key) = auth_header.strip_prefix("ApiKey ") {
            if validate_api_key(header_key, &self.api_key) {
                return Some(AuthResult {
                    subject: "api-key".to_string(),
                    role: "admin".to_string(),
                    scopes: vec!["scope_root".to_string()],
                    user_id: None,
                });
            }
        }

        None
    }
}

/// Hash a password using Argon2id with OWASP-recommended parameters.
/// m=65536 (64 MiB), t=3 iterations, p=4 parallelism — matches OWASP recommendation #2.
pub fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    use argon2::password_hash::rand_core::OsRng;
    use argon2::password_hash::SaltString;
    use argon2::{Algorithm, Argon2, Params, PasswordHasher, Version};

    let salt = SaltString::generate(&mut OsRng);
    let params = Params::new(65536, 3, 4, None)
        .map_err(|_| argon2::password_hash::Error::ParamNameInvalid)?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let hash = argon2.hash_password(password.as_bytes(), &salt)?;
    Ok(hash.to_string())
}

/// Verify a password against an Argon2id hash.
pub fn verify_password(password: &str, hash: &str) -> bool {
    use argon2::{Argon2, PasswordHash, PasswordVerifier};

    let Ok(parsed_hash) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok()
}

/// Request to obtain a JWT token.
#[derive(Deserialize)]
pub struct TokenRequest {
    pub api_key: Option<String>,
    /// Username for user-based auth
    pub username: Option<String>,
    /// Password for user-based auth
    pub password: Option<String>,
    pub subject: Option<String>,
    pub role: Option<String>,
    pub scopes: Option<Vec<String>>,
}

/// Response containing the JWT token.
#[derive(Serialize)]
pub struct TokenResponse {
    pub token: String,
    pub expires_in: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

/// Request to revoke a JWT token.
#[derive(Deserialize)]
pub struct RevokeTokenRequest {
    pub token: String,
}

/// User registration request.
#[derive(Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub password: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
}

/// User registration response.
#[derive(Serialize)]
pub struct RegisterResponse {
    pub user_id: String,
    pub username: String,
    pub role: String,
}

/// User profile response.
#[derive(Serialize)]
pub struct UserProfileResponse {
    pub user_id: String,
    pub username: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub role: String,
    pub created_at: String,
}

/// Admin user list response.
#[derive(Serialize)]
pub struct UserListResponse {
    pub users: Vec<UserProfileResponse>,
}

/// Admin update user role request.
#[derive(Deserialize)]
pub struct UpdateUserRoleRequest {
    pub role: String,
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

    #[tokio::test]
    async fn test_auth_handler_with_api_key() {
        let handler = AuthHandler::new("test-key".to_string(), None, 3600);
        let result = handler.authenticate("", Some("test-key".to_string())).await;
        assert!(result.is_some());
        let auth = result.unwrap();
        assert_eq!(auth.role, "admin");
        assert!(auth.user_id.is_none());
    }

    #[tokio::test]
    async fn test_auth_handler_invalid_api_key() {
        let handler = AuthHandler::new("test-key".to_string(), None, 3600);
        let result = handler
            .authenticate("", Some("wrong-key".to_string()))
            .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_auth_handler_with_jwt() {
        let jwt_secret = "test-secret";
        let handler = AuthHandler::new("api-key".to_string(), Some(jwt_secret.to_string()), 3600);

        let jwt_service = handler.jwt_service.as_ref().unwrap();
        let token = jwt_service
            .generate_token("user1", "admin", vec!["scope_root".to_string()], None)
            .unwrap();

        let result = handler
            .authenticate(&format!("Bearer {}", token), None)
            .await;
        assert!(result.is_some());
        let auth = result.unwrap();
        assert_eq!(auth.subject, "user1");
        assert_eq!(auth.role, "admin");
    }

    #[tokio::test]
    async fn test_auth_handler_with_user_jwt() {
        let jwt_secret = "test-secret";
        let handler = AuthHandler::new("api-key".to_string(), Some(jwt_secret.to_string()), 3600);

        let jwt_service = handler.jwt_service.as_ref().unwrap();
        let token = jwt_service
            .generate_token(
                "alice",
                "user",
                vec!["scope_root".to_string()],
                Some("user_alice".to_string()),
            )
            .unwrap();

        let result = handler
            .authenticate(&format!("Bearer {}", token), None)
            .await;
        assert!(result.is_some());
        let auth = result.unwrap();
        assert_eq!(auth.subject, "alice");
        assert_eq!(auth.role, "user");
        assert_eq!(auth.user_id, Some("user_alice".to_string()));
    }

    #[test]
    fn test_password_hashing() {
        let password = "my-secure-password";
        let hash = hash_password(password).unwrap();
        assert!(verify_password(password, &hash));
        assert!(!verify_password("wrong-password", &hash));
    }
}
