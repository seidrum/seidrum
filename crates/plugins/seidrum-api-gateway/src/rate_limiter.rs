//! In-memory token bucket rate limiter.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// Token bucket entry tracking available tokens and last refill time.
#[derive(Debug, Clone)]
struct TokenBucket {
    tokens: f64,
    last_refill: u64,
}

/// Rate limiter configuration.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Requests per minute for regular users
    pub regular_rpm: u32,
    /// Requests per minute for admin users
    pub admin_rpm: u32,
    /// Cleanup interval in seconds
    pub cleanup_interval_secs: u64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            regular_rpm: 60,
            admin_rpm: 300,
            cleanup_interval_secs: 300, // 5 minutes
        }
    }
}

/// Simple in-memory token bucket rate limiter.
/// Tracks per-client (identified by subject) rate limits.
#[derive(Clone)]
pub struct RateLimiter {
    buckets: Arc<RwLock<HashMap<String, TokenBucket>>>,
    config: RateLimitConfig,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            buckets: Arc::new(RwLock::new(HashMap::new())),
            config,
        }
    }

    /// Check if a request from the given subject is allowed.
    /// Returns (allowed, remaining_tokens, retry_after_secs).
    pub async fn check_rate_limit(
        &self,
        subject: &str,
        is_admin: bool,
    ) -> (bool, u32, Option<u32>) {
        let rpm = if is_admin {
            self.config.admin_rpm
        } else {
            self.config.regular_rpm
        };
        let tokens_per_sec = rpm as f64 / 60.0;

        let now = unix_timestamp_secs();
        let mut buckets = self.buckets.write().await;

        let bucket = buckets
            .entry(subject.to_string())
            .or_insert_with(|| TokenBucket {
                tokens: rpm as f64,
                last_refill: now,
            });

        // Refill tokens based on elapsed time
        let elapsed = now.saturating_sub(bucket.last_refill);
        bucket.tokens = (bucket.tokens + (elapsed as f64 * tokens_per_sec)).min(rpm as f64);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            let remaining = bucket.tokens.floor() as u32;
            debug!(subject, remaining, "Rate limit check passed");
            (true, remaining, None)
        } else {
            // Calculate retry-after: time until next token is available
            let retry_after = ((1.0 - bucket.tokens) / tokens_per_sec).ceil() as u32;
            warn!(subject, retry_after, "Rate limit exceeded");
            (false, 0, Some(retry_after))
        }
    }

    /// Cleanup stale entries (subjects not accessed for a while).
    pub async fn cleanup_stale_entries(&self, ttl_secs: u64) {
        let now = unix_timestamp_secs();
        let mut buckets = self.buckets.write().await;
        let before = buckets.len();

        buckets.retain(|_, bucket| now.saturating_sub(bucket.last_refill) < ttl_secs);

        let after = buckets.len();
        if before != after {
            debug!(
                removed = before - after,
                "Cleaned up stale rate limit entries"
            );
        }
    }
}

fn unix_timestamp_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_rate_limit_allows_requests() {
        let config = RateLimitConfig {
            regular_rpm: 2,
            admin_rpm: 2,
            cleanup_interval_secs: 300,
        };
        let limiter = RateLimiter::new(config);

        // First request should be allowed
        let (allowed, remaining, retry) = limiter.check_rate_limit("user1", false).await;
        assert!(allowed);
        assert_eq!(remaining, 1);
        assert_eq!(retry, None);

        // Second request should be allowed
        let (allowed, remaining, retry) = limiter.check_rate_limit("user1", false).await;
        assert!(allowed);
        assert_eq!(remaining, 0);
        assert_eq!(retry, None);

        // Third request should be rejected
        let (allowed, remaining, retry) = limiter.check_rate_limit("user1", false).await;
        assert!(!allowed);
        assert_eq!(remaining, 0);
        assert!(retry.is_some());
    }

    #[tokio::test]
    async fn test_admin_higher_limit() {
        let config = RateLimitConfig {
            regular_rpm: 1,
            admin_rpm: 2,
            cleanup_interval_secs: 300,
        };
        let limiter = RateLimiter::new(config);

        // Admin should have higher limit
        let (allowed1, _, _) = limiter.check_rate_limit("admin", true).await;
        let (allowed2, _, _) = limiter.check_rate_limit("admin", true).await;
        assert!(allowed1);
        assert!(allowed2);

        // Regular user should have lower limit
        let (allowed1, _, _) = limiter.check_rate_limit("user", false).await;
        let (allowed2, _, _) = limiter.check_rate_limit("user", false).await;
        assert!(allowed1);
        assert!(!allowed2);
    }
}
