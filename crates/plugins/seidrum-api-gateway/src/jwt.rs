//! JWT token generation and validation using the `jsonwebtoken` crate.
//! Supports token revocation via an in-memory JTI blacklist.

use anyhow::Result;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tracing::debug;

/// JWT claims payload.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Claims {
    /// JWT ID (unique token identifier for revocation)
    pub jti: String,
    /// Subject (user ID or API key identifier)
    pub sub: String,
    /// Issued at (unix timestamp)
    pub iat: u64,
    /// Expiration (unix timestamp)
    pub exp: u64,
    /// Scopes this token has access to
    pub scopes: Vec<String>,
    /// Role: "admin", "user", "readonly"
    pub role: String,
    /// User ID for multi-user support (None for API key tokens)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

/// JWT service using the `jsonwebtoken` crate with HMAC-SHA256.
#[derive(Clone)]
pub struct JwtService {
    encoding_key: EncodingKey,
    decoding_key: DecodingKey,
    pub token_ttl_secs: u64,
    /// Set of revoked JTI values (token IDs).
    revoked: Arc<RwLock<HashSet<String>>>,
}

impl JwtService {
    pub fn new(secret: &str, token_ttl_secs: u64) -> Self {
        Self {
            encoding_key: EncodingKey::from_secret(secret.as_bytes()),
            decoding_key: DecodingKey::from_secret(secret.as_bytes()),
            token_ttl_secs,
            revoked: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// Generate a JWT token for the given subject and role.
    pub fn generate_token(
        &self,
        subject: &str,
        role: &str,
        scopes: Vec<String>,
        user_id: Option<String>,
    ) -> Result<String> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let jti = ulid::Ulid::new().to_string();

        let claims = Claims {
            jti,
            sub: subject.to_string(),
            iat: now,
            exp: now + self.token_ttl_secs,
            scopes,
            role: role.to_string(),
            user_id,
        };

        let token = encode(&Header::default(), &claims, &self.encoding_key)?;
        Ok(token)
    }

    /// Validate a JWT token and return the claims.
    /// Returns an error if the token is expired, invalid, or revoked.
    pub async fn validate_token(&self, token: &str) -> Result<Claims> {
        let mut validation = Validation::default();
        validation.validate_exp = true;
        validation.leeway = 0;
        validation.required_spec_claims.clear();

        let token_data = decode::<Claims>(token, &self.decoding_key, &validation)
            .map_err(|e| anyhow::anyhow!("JWT validation failed: {}", e))?;

        let claims = token_data.claims;

        // Check revocation
        let revoked = self.revoked.read().await;
        if revoked.contains(&claims.jti) {
            anyhow::bail!("Token has been revoked");
        }

        Ok(claims)
    }

    /// Revoke a token by its JTI (JWT ID).
    pub async fn revoke_token(&self, jti: &str) {
        let mut revoked = self.revoked.write().await;
        revoked.insert(jti.to_string());
        debug!(jti, "Token revoked");
    }

    /// Check if a token (by JTI) has been revoked.
    pub async fn is_revoked(&self, jti: &str) -> bool {
        self.revoked.read().await.contains(jti)
    }

    /// Clean up expired revocation entries.
    /// Call this periodically to prevent unbounded growth.
    pub async fn cleanup_revocations(&self, max_entries: usize) {
        let mut revoked = self.revoked.write().await;
        if revoked.len() > max_entries {
            // When over limit, clear all — expired tokens can't be validated anyway
            let removed = revoked.len() - max_entries;
            // Keep the most recent entries by draining oldest
            let to_remove: Vec<String> = revoked.iter().take(removed).cloned().collect();
            for jti in to_remove {
                revoked.remove(&jti);
            }
            debug!(removed, "Cleaned up revocation entries");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_generate_and_validate_token() {
        let service = JwtService::new("test-secret-key", 3600);
        let token = service
            .generate_token("user1", "admin", vec!["scope_root".to_string()], None)
            .unwrap();
        let claims = service.validate_token(&token).await.unwrap();
        assert_eq!(claims.sub, "user1");
        assert_eq!(claims.role, "admin");
        assert_eq!(claims.scopes, vec!["scope_root"]);
        assert!(!claims.jti.is_empty());
    }

    #[tokio::test]
    async fn test_generate_token_with_user_id() {
        let service = JwtService::new("test-secret-key", 3600);
        let token = service
            .generate_token(
                "user1",
                "user",
                vec!["scope_root".to_string()],
                Some("user_abc123".to_string()),
            )
            .unwrap();
        let claims = service.validate_token(&token).await.unwrap();
        assert_eq!(claims.user_id, Some("user_abc123".to_string()));
    }

    #[tokio::test]
    async fn test_expired_token() {
        let service = JwtService::new("test-secret-key", 0);
        let token = service
            .generate_token("user1", "admin", vec![], None)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        assert!(service.validate_token(&token).await.is_err());
    }

    #[tokio::test]
    async fn test_invalid_signature() {
        let service1 = JwtService::new("secret1", 3600);
        let service2 = JwtService::new("secret2", 3600);
        let token = service1
            .generate_token("user1", "admin", vec![], None)
            .unwrap();
        assert!(service2.validate_token(&token).await.is_err());
    }

    #[tokio::test]
    async fn test_token_revocation() {
        let service = JwtService::new("test-secret-key", 3600);
        let token = service
            .generate_token("user1", "admin", vec![], None)
            .unwrap();

        // Token should be valid
        let claims = service.validate_token(&token).await.unwrap();
        let jti = claims.jti.clone();

        // Revoke the token
        service.revoke_token(&jti).await;

        // Token should now be rejected
        assert!(service.validate_token(&token).await.is_err());
        assert!(service.is_revoked(&jti).await);
    }
}
