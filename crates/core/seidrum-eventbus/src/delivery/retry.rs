//! Background retry task for failed deliveries.
//!
//! Periodically polls the event store for retryable deliveries,
//! re-attempts delivery, and handles backoff logic.

use crate::storage::{EventStore, RetryableDelivery};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info};

/// Exponential backoff with jitter for retries.
pub fn calculate_backoff(attempt: u32, initial_ms: u64, max_ms: u64) -> Duration {
    let base_ms = initial_ms.saturating_mul(2_u64.saturating_pow(attempt));
    let capped_ms = base_ms.min(max_ms);
    // Jitter is subtracted (not added) so the result never exceeds capped_ms.
    let jitter_range = (capped_ms / 4).max(1);
    let jitter_ms = rand::random_range(0..jitter_range);
    Duration::from_millis(capped_ms - jitter_ms)
}

/// Background task for retrying failed deliveries.
pub struct RetryTask {
    store: Arc<dyn EventStore>,
    max_attempts: u32,
    poll_interval: Duration,
    initial_backoff_ms: u64,
    max_backoff_ms: u64,
}

impl RetryTask {
    /// Create a new retry task.
    pub fn new(
        store: Arc<dyn EventStore>,
        max_attempts: u32,
        poll_interval: Duration,
        initial_backoff_ms: u64,
        max_backoff_ms: u64,
    ) -> Self {
        Self {
            store,
            max_attempts,
            poll_interval,
            initial_backoff_ms,
            max_backoff_ms,
        }
    }

    /// Run the retry task continuously.
    /// This task polls the store periodically and retries failed deliveries.
    pub async fn run(self) -> ! {
        info!("Retry task starting");

        loop {
            tokio::time::sleep(self.poll_interval).await;

            if let Err(e) = self.poll_and_retry().await {
                error!("Error during retry poll: {}", e);
            }
        }
    }

    async fn poll_and_retry(&self) -> crate::Result<()> {
        let retryables = self
            .store
            .query_retryable(self.max_attempts, 100)
            .await
            .map_err(crate::EventBusError::Storage)?;

        if retryables.is_empty() {
            return Ok(());
        }

        debug!("Found {} retryable deliveries", retryables.len());

        for delivery in retryables {
            self.retry_delivery(&delivery).await;
        }

        Ok(())
    }

    async fn retry_delivery(&self, delivery: &RetryableDelivery) {
        debug!(
            "Retrying delivery for seq={} subscriber={} attempt={}",
            delivery.seq, delivery.subscriber_id, delivery.attempts
        );

        // In a real implementation, we would:
        // 1. Look up the delivery channel for this subscriber
        // 2. Call deliver() on it
        // 3. Record the result
        // For now, we log and mark for next retry cycle

        let backoff = calculate_backoff(
            delivery.attempts,
            self.initial_backoff_ms,
            self.max_backoff_ms,
        );
        debug!(
            "Next retry scheduled in {:?} for seq={}",
            backoff, delivery.seq
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_backoff() {
        let d1 = calculate_backoff(0, 100, 30000);
        // attempt 0: base=100, jitter subtracts up to 25 → 75..100
        assert!(d1 >= Duration::from_millis(75));
        assert!(d1 <= Duration::from_millis(100));

        let d2 = calculate_backoff(1, 100, 30000);
        // attempt 1: base=200, jitter subtracts up to 50 → 150..200
        assert!(d2 >= Duration::from_millis(150));
        assert!(d2 <= Duration::from_millis(200));

        let d3 = calculate_backoff(10, 100, 30000);
        // attempt 10: base capped at 30000, jitter subtracts up to 7500 → 22500..30000
        assert!(d3 <= Duration::from_millis(30000));
        assert!(d3 >= Duration::from_millis(22500));
    }

    #[test]
    fn test_calculate_backoff_jitter() {
        let mut durations = Vec::new();
        for _ in 0..10 {
            durations.push(calculate_backoff(1, 100, 30000));
        }

        // All should be different due to jitter
        let unique_count = durations
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len();
        assert!(unique_count > 1); // At least 2 different values due to jitter
    }
}
