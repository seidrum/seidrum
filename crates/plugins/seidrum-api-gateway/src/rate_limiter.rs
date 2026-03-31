//! Token bucket rate limiter with per-user limits and state persistence.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use seidrum_common::events::{
    RateLimiterBucket, RateLimiterState, StorageGetRequest, StorageGetResponse, StorageSetRequest,
    StorageSetResponse,
};
use seidrum_common::nats_utils::NatsClient;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

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

const STORAGE_PLUGIN_ID: &str = "api-gateway";
const STORAGE_NAMESPACE: &str = "rate_limiter";
const STORAGE_KEY: &str = "state";

/// Token bucket rate limiter with per-user limits and persistence.
/// Tracks per-client (identified by subject) rate limits.
#[derive(Clone)]
pub struct RateLimiter {
    buckets: Arc<RwLock<HashMap<String, TokenBucket>>>,
    config: RateLimitConfig,
    /// Per-user RPM overrides (user_id -> rpm)
    user_rpm_overrides: Arc<RwLock<HashMap<String, u32>>>,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            buckets: Arc::new(RwLock::new(HashMap::new())),
            config,
            user_rpm_overrides: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Set a per-user RPM override.
    pub async fn set_user_rpm(&self, user_id: &str, rpm: u32) {
        self.user_rpm_overrides
            .write()
            .await
            .insert(user_id.to_string(), rpm);
    }

    /// Get the effective RPM for a subject, considering per-user overrides.
    async fn effective_rpm(&self, subject: &str, is_admin: bool) -> u32 {
        // Check per-user override first
        if let Some(&rpm) = self.user_rpm_overrides.read().await.get(subject) {
            return rpm;
        }
        if is_admin {
            self.config.admin_rpm
        } else {
            self.config.regular_rpm
        }
    }

    /// Check if a request from the given subject is allowed.
    /// Returns (allowed, remaining_tokens, retry_after_secs).
    pub async fn check_rate_limit(
        &self,
        subject: &str,
        is_admin: bool,
    ) -> (bool, u32, Option<u32>) {
        let rpm = self.effective_rpm(subject, is_admin).await;
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

    /// Save rate limiter state to plugin storage via NATS for persistence across restarts.
    pub async fn save_state(&self, nats: &NatsClient) {
        let buckets = self.buckets.read().await;
        let state = RateLimiterState {
            buckets: buckets
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        RateLimiterBucket {
                            tokens: v.tokens,
                            last_refill: v.last_refill,
                        },
                    )
                })
                .collect(),
            saved_at: chrono::Utc::now(),
        };
        drop(buckets); // Release read lock before NATS call

        let req = StorageSetRequest {
            plugin_id: STORAGE_PLUGIN_ID.to_string(),
            namespace: STORAGE_NAMESPACE.to_string(),
            key: STORAGE_KEY.to_string(),
            value: serde_json::to_value(&state).unwrap_or_default(),
        };

        match tokio::time::timeout(
            Duration::from_secs(5),
            nats.request::<_, StorageSetResponse>("storage.set", &req),
        )
        .await
        {
            Ok(Ok(_)) => {
                debug!(
                    bucket_count = state.buckets.len(),
                    "Rate limiter state saved"
                );
            }
            Ok(Err(e)) => {
                warn!(error = %e, "Failed to save rate limiter state");
            }
            Err(_) => {
                warn!("Timeout saving rate limiter state");
            }
        }
    }

    /// Restore rate limiter state from plugin storage via NATS.
    pub async fn restore_state(&self, nats: &NatsClient) {
        let req = StorageGetRequest {
            plugin_id: STORAGE_PLUGIN_ID.to_string(),
            namespace: STORAGE_NAMESPACE.to_string(),
            key: STORAGE_KEY.to_string(),
        };

        match tokio::time::timeout(
            Duration::from_secs(5),
            nats.request::<_, StorageGetResponse>("storage.get", &req),
        )
        .await
        {
            Ok(Ok(resp)) => {
                if let Some(value) = resp.value {
                    match serde_json::from_value::<RateLimiterState>(value) {
                        Ok(state) => {
                            let now = unix_timestamp_secs();
                            let mut buckets = self.buckets.write().await;

                            for (subject, bucket) in state.buckets {
                                // Only restore entries that are less than 10 minutes old
                                if now.saturating_sub(bucket.last_refill) < 600 {
                                    buckets.insert(
                                        subject,
                                        TokenBucket {
                                            tokens: bucket.tokens,
                                            last_refill: bucket.last_refill,
                                        },
                                    );
                                }
                            }

                            info!(
                                restored = buckets.len(),
                                "Rate limiter state restored from storage"
                            );
                        }
                        Err(e) => {
                            warn!(error = %e, "Failed to parse rate limiter state");
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                debug!(error = %e, "No rate limiter state to restore (first run or storage unavailable)");
            }
            Err(_) => {
                debug!("Timeout restoring rate limiter state (storage may not be ready)");
            }
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

    #[tokio::test]
    async fn test_per_user_rpm_override() {
        let config = RateLimitConfig {
            regular_rpm: 1,
            admin_rpm: 10,
            cleanup_interval_secs: 300,
        };
        let limiter = RateLimiter::new(config);

        // Set user-specific RPM
        limiter.set_user_rpm("special_user", 3).await;

        // Should get 3 requests (custom RPM) instead of 1 (regular)
        let (a1, _, _) = limiter.check_rate_limit("special_user", false).await;
        let (a2, _, _) = limiter.check_rate_limit("special_user", false).await;
        let (a3, _, _) = limiter.check_rate_limit("special_user", false).await;
        let (a4, _, _) = limiter.check_rate_limit("special_user", false).await;
        assert!(a1);
        assert!(a2);
        assert!(a3);
        assert!(!a4);
    }
}
