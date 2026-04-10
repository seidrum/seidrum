//! Background retry task for failed deliveries.
//!
//! Periodically polls the event store for retryable deliveries and re-attempts
//! them via the [`crate::dispatch::DispatchEngine`]. The engine looks up the
//! live subscription, routes to the correct delivery channel, and reports
//! the outcome. Successful retries are recorded as `Delivered`; transient
//! failures are recorded as `Failed` (retried after exponential backoff);
//! permanent failures and exhausted attempts are recorded as `DeadLettered`.

use crate::dispatch::engine::RetryOutcome;
use crate::dispatch::DispatchEngine;
use crate::storage::{DeliveryStatus, EventStatus, EventStore, RetryableDelivery};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

/// Configuration for delivery retries (backoff parameters).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    /// Maximum number of delivery attempts before giving up.
    pub max_attempts: u32,
    /// Initial backoff duration in milliseconds.
    pub initial_backoff_ms: u64,
    /// Maximum backoff duration in milliseconds.
    pub max_backoff_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_backoff_ms: 100,
            max_backoff_ms: 30000,
        }
    }
}

/// Canonical exponential backoff with subtractive jitter.
///
/// `attempt` is the *next* attempt number (1-indexed). Returns
/// `initial_ms × 2^(attempt-1)` capped at `max_ms`, with up to 25%
/// subtractive jitter.
///
/// Used by both the dispatch engine (when recording an initial failure)
/// and the retry task (when recording a retry failure) so all backoff
/// timing flows through one source of truth.
pub fn calculate_backoff(attempt: u32, initial_ms: u64, max_ms: u64) -> Duration {
    // Subtract 1 so the first attempt waits initial_ms, not 2*initial_ms.
    let exp = attempt.saturating_sub(1);
    let base_ms = initial_ms.saturating_mul(2_u64.saturating_pow(exp));
    let capped_ms = base_ms.min(max_ms);
    let jitter_range = (capped_ms / 4).max(1);
    let jitter_ms = rand::random_range(0..jitter_range);
    Duration::from_millis(capped_ms - jitter_ms)
}

/// Compute the next retry timestamp (unix-millis) for a delivery that just
/// failed and now has `attempts` total failures recorded. Convenience wrapper
/// over [`calculate_backoff`] that adds the backoff to the current time.
pub fn next_retry_after(attempts: u32, config: &RetryConfig) -> u64 {
    let backoff = calculate_backoff(attempts, config.initial_backoff_ms, config.max_backoff_ms);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    now + backoff.as_millis() as u64
}

/// Background task for retrying failed deliveries.
///
/// Polls the store on a fixed interval, calls
/// [`DispatchEngine::retry_to_subscriber`] for each retryable delivery,
/// and updates the store with the outcome.
///
/// All backoff timing flows through [`calculate_backoff`] / [`next_retry_after`]
/// using the [`RetryConfig`] supplied at construction.
pub struct RetryTask {
    store: Arc<dyn EventStore>,
    engine: Arc<DispatchEngine>,
    config: Arc<RetryConfig>,
    poll_interval: Duration,
    query_limit: usize,
    shutdown_rx: watch::Receiver<bool>,
}

impl RetryTask {
    /// Create a new retry task.
    pub fn new(
        store: Arc<dyn EventStore>,
        engine: Arc<DispatchEngine>,
        config: Arc<RetryConfig>,
        poll_interval: Duration,
        query_limit: usize,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            store,
            engine,
            config,
            poll_interval,
            query_limit,
            shutdown_rx,
        }
    }

    /// Run the retry task continuously until a shutdown signal is received.
    /// This task polls the store periodically and retries failed deliveries.
    pub async fn run(self) {
        info!("Retry task starting");

        // Check pre-signaled shutdown to avoid waiting a full poll interval.
        if *self.shutdown_rx.borrow() {
            info!("Retry task shutting down (pre-signaled)");
            return;
        }

        // Move shutdown_rx out so the select! arm can take a mutable
        // borrow without conflicting with the &self borrow used by
        // poll_and_retry(). Use changed() (which returns a Send result)
        // rather than wait_for (which returns a non-Send watch::Ref).
        let mut shutdown_rx = self.shutdown_rx.clone();

        loop {
            tokio::select! {
                _ = tokio::time::sleep(self.poll_interval) => {
                    if let Err(e) = self.poll_and_retry().await {
                        error!("Error during retry poll: {}", e);
                    }
                }
                result = shutdown_rx.changed() => {
                    if result.is_err() || *shutdown_rx.borrow() {
                        info!("Retry task shutting down");
                        break;
                    }
                }
            }
        }
    }

    async fn poll_and_retry(&self) -> crate::Result<()> {
        let retryables = self
            .store
            .query_retryable(self.config.max_attempts, self.query_limit)
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
            seq = delivery.seq,
            subscriber = delivery.subscriber_id,
            attempt = delivery.attempts,
            "retrying delivery"
        );

        // Defensive: query_retryable filters attempts < max_attempts.
        if delivery.attempts >= self.config.max_attempts {
            self.dead_letter(delivery, "max attempts exceeded").await;
            return;
        }

        // Attempt the retry via the engine, propagating reply_subject so
        // request/reply handlers see a working Replier.
        let result = self
            .engine
            .retry_to_subscriber(
                &delivery.subscriber_id,
                &delivery.subject,
                &delivery.payload,
                delivery.reply_subject.clone(),
                delivery.seq,
            )
            .await;

        match result {
            Ok(()) => {
                debug!(
                    seq = delivery.seq,
                    subscriber = delivery.subscriber_id,
                    "retry succeeded"
                );
                if let Err(e) = self
                    .store
                    .record_delivery(
                        delivery.seq,
                        &delivery.subscriber_id,
                        DeliveryStatus::Delivered,
                        None,
                        None,
                    )
                    .await
                {
                    warn!(seq = delivery.seq, error = %e, "failed to record retry success");
                }
                self.maybe_finalize_event(delivery.seq).await;
            }
            Err(RetryOutcome::Transient(msg)) => {
                let new_attempts = delivery.attempts + 1;
                debug!(
                    seq = delivery.seq,
                    subscriber = delivery.subscriber_id,
                    attempt = new_attempts,
                    error = %msg,
                    "retry failed (transient)"
                );
                // Compute next_retry from the new attempt count using the
                // canonical backoff helper.
                let next_retry = next_retry_after(new_attempts, &self.config);
                if let Err(e) = self
                    .store
                    .record_delivery(
                        delivery.seq,
                        &delivery.subscriber_id,
                        DeliveryStatus::Failed,
                        Some(msg.clone()),
                        Some(next_retry),
                    )
                    .await
                {
                    warn!(seq = delivery.seq, error = %e, "failed to record retry failure");
                }
                // If this attempt was the last allowed one, dead-letter
                // immediately. Otherwise the entry would stop being returned
                // by query_retryable (which filters attempts < max_attempts)
                // and would be stuck in Failed state forever.
                if new_attempts >= self.config.max_attempts {
                    self.dead_letter(
                        delivery,
                        &format!(
                            "max attempts ({}) exhausted: {}",
                            self.config.max_attempts, msg
                        ),
                    )
                    .await;
                }
            }
            Err(RetryOutcome::Permanent(msg)) => {
                debug!(
                    seq = delivery.seq,
                    subscriber = delivery.subscriber_id,
                    error = %msg,
                    "retry failed (permanent), dead-lettering"
                );
                self.dead_letter(delivery, &msg).await;
            }
        }
    }

    /// Mark a delivery as dead-lettered and update the parent event status
    /// if all subscribers are now in a terminal state.
    async fn dead_letter(&self, delivery: &RetryableDelivery, reason: &str) {
        // Use a single canonical reason format so log/error parsing is
        // consistent across all dead-letter sources.
        let formatted = format!("dead-lettered: {}", reason);
        if let Err(e) = self
            .store
            .record_delivery(
                delivery.seq,
                &delivery.subscriber_id,
                DeliveryStatus::DeadLettered,
                Some(formatted),
                None,
            )
            .await
        {
            warn!(seq = delivery.seq, error = %e, "failed to record dead-letter");
        }
        self.maybe_finalize_event(delivery.seq).await;
    }

    /// If every delivery for an event is in a terminal state (Delivered or
    /// DeadLettered), update the event status accordingly.
    ///
    /// Uses [`EventStore::get`] to load the specific event, avoiding the
    /// status-based scan + bounded-window failure modes of the previous
    /// implementation.
    async fn maybe_finalize_event(&self, seq: u64) {
        let event = match self.store.get(seq).await {
            Ok(Some(e)) => e,
            Ok(None) => return, // Compacted away
            Err(e) => {
                warn!(seq = seq, error = %e, "failed to load event for finalization");
                return;
            }
        };

        // Check if every delivery is terminal (Delivered or DeadLettered).
        let all_terminal = event.deliveries.iter().all(|d| {
            matches!(
                d.status,
                DeliveryStatus::Delivered | DeliveryStatus::DeadLettered
            )
        });
        if !all_terminal {
            return;
        }

        let any_dead = event
            .deliveries
            .iter()
            .any(|d| d.status == DeliveryStatus::DeadLettered);
        let final_status = if any_dead {
            EventStatus::DeadLettered
        } else {
            EventStatus::Delivered
        };
        if let Err(e) = self.store.update_status(seq, final_status).await {
            warn!(seq = seq, error = %e, "failed to finalize event status");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retry_config_default() {
        let config = RetryConfig::default();
        assert_eq!(config.max_attempts, 5);
        assert_eq!(config.initial_backoff_ms, 100);
        assert_eq!(config.max_backoff_ms, 30000);
    }

    #[test]
    fn test_calculate_backoff() {
        // attempt 1 (first retry): base=100, jitter subtracts up to 25 → 75..100
        let d1 = calculate_backoff(1, 100, 30000);
        assert!(d1 >= Duration::from_millis(75));
        assert!(d1 <= Duration::from_millis(100));

        // attempt 2: base=200, jitter subtracts up to 50 → 150..200
        let d2 = calculate_backoff(2, 100, 30000);
        assert!(d2 >= Duration::from_millis(150));
        assert!(d2 <= Duration::from_millis(200));

        // attempt 11: 100 * 2^10 = 102400, capped at 30000
        let d11 = calculate_backoff(11, 100, 30000);
        assert!(d11 <= Duration::from_millis(30000));
        assert!(d11 >= Duration::from_millis(22500));
    }

    #[test]
    fn test_calculate_backoff_jitter() {
        let mut durations = Vec::new();
        for _ in 0..10 {
            durations.push(calculate_backoff(2, 100, 30000));
        }

        // All should be different due to jitter
        let unique_count = durations
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len();
        assert!(unique_count > 1); // At least 2 different values due to jitter
    }
}
