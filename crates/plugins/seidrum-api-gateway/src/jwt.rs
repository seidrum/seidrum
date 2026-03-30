//! JWT token generation and validation for API gateway authentication.

use std::time::{SystemTime, UNIX_EPOCH};
use anyhow::Result;
use serde::{Deserialize, Serialize};
// tracing available if needed for debug/warn

/// JWT claims payload.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Claims {
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
}

/// Simple HMAC-SHA256 based JWT implementation.
/// In production, use a proper JWT library — this is a minimal implementation
/// to avoid adding heavy dependencies.
#[derive(Clone)]
pub struct JwtService {
    secret: Vec<u8>,
    pub token_ttl_secs: u64,
}

impl JwtService {
    pub fn new(secret: &str, token_ttl_secs: u64) -> Self {
        Self {
            secret: secret.as_bytes().to_vec(),
            token_ttl_secs,
        }
    }

    /// Generate a JWT token for the given subject and role.
    pub fn generate_token(&self, subject: &str, role: &str, scopes: Vec<String>) -> Result<String> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let claims = Claims {
            sub: subject.to_string(),
            iat: now,
            exp: now + self.token_ttl_secs,
            scopes,
            role: role.to_string(),
        };

        let header = base64_url_encode(r#"{"alg":"HS256","typ":"JWT"}"#.as_bytes());
        let payload = base64_url_encode(&serde_json::to_vec(&claims)?);
        let signature_input = format!("{}.{}", header, payload);
        let signature = hmac_sha256(&self.secret, signature_input.as_bytes());
        let sig_encoded = base64_url_encode(&signature);

        Ok(format!("{}.{}.{}", header, payload, sig_encoded))
    }

    /// Validate a JWT token and return the claims.
    pub fn validate_token(&self, token: &str) -> Result<Claims> {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 {
            anyhow::bail!("Invalid token format");
        }

        // Verify signature
        let signature_input = format!("{}.{}", parts[0], parts[1]);
        let expected_signature = hmac_sha256(&self.secret, signature_input.as_bytes());
        let expected_encoded = base64_url_encode(&expected_signature);

        if parts[2] != expected_encoded {
            anyhow::bail!("Invalid token signature");
        }

        // Decode claims
        let payload_bytes = base64_url_decode(parts[1])?;
        let claims: Claims = serde_json::from_slice(&payload_bytes)?;

        // Check expiration
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        if claims.exp < now {
            anyhow::bail!("Token expired");
        }

        Ok(claims)
    }
}

fn base64_url_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn base64_url_decode(s: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s)?)
}

/// Simple HMAC-SHA256 using ring or manual implementation.
/// For simplicity, use a basic implementation with the sha2 crate.
fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    use sha2::{Sha256, Digest};

    // HMAC: H((K' XOR opad) || H((K' XOR ipad) || message))
    let block_size = 64;
    let mut k_prime = vec![0u8; block_size];
    if key.len() > block_size {
        let mut hasher = Sha256::new();
        hasher.update(key);
        let hash = hasher.finalize();
        k_prime[..32].copy_from_slice(&hash);
    } else {
        k_prime[..key.len()].copy_from_slice(key);
    }

    let mut ipad = vec![0x36u8; block_size];
    let mut opad = vec![0x5cu8; block_size];
    for i in 0..block_size {
        ipad[i] ^= k_prime[i];
        opad[i] ^= k_prime[i];
    }

    let mut inner_hasher = Sha256::new();
    inner_hasher.update(&ipad);
    inner_hasher.update(data);
    let inner_hash = inner_hasher.finalize();

    let mut outer_hasher = Sha256::new();
    outer_hasher.update(&opad);
    outer_hasher.update(&inner_hash);
    outer_hasher.finalize().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_and_validate_token() {
        let service = JwtService::new("test-secret-key", 3600);
        let token = service.generate_token("user1", "admin", vec!["scope_root".to_string()]).unwrap();
        let claims = service.validate_token(&token).unwrap();
        assert_eq!(claims.sub, "user1");
        assert_eq!(claims.role, "admin");
        assert_eq!(claims.scopes, vec!["scope_root"]);
    }

    #[test]
    fn test_expired_token() {
        let service = JwtService::new("test-secret-key", 0); // 0 second TTL
        let token = service.generate_token("user1", "admin", vec![]).unwrap();
        // Token should be expired immediately
        std::thread::sleep(std::time::Duration::from_secs(1));
        assert!(service.validate_token(&token).is_err());
    }

    #[test]
    fn test_invalid_signature() {
        let service1 = JwtService::new("secret1", 3600);
        let service2 = JwtService::new("secret2", 3600);
        let token = service1.generate_token("user1", "admin", vec![]).unwrap();
        assert!(service2.validate_token(&token).is_err());
    }
}
