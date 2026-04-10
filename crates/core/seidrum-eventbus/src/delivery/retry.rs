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
///
/// Polls the store on a fixed interval, calls
/// [`DispatchEngine::retry_to_subscriber`] for each retryable delivery,
/// and updates the store with the outcome.
///
/// Backoff timing is computed by the store implementation in
/// `record_delivery` (currently 100ms × 2^attempts capped at 30s).
pub struct RetryTask {
    store: Arc<dyn EventStore>,
    engine: Arc<DispatchEngine>,
    max_attempts: u32,
    poll_interval: Duration,
    shutdown_rx: watch::Receiver<bool>,
}

impl RetryTask {
    /// Create a new retry task.
    pub fn new(
        store: Arc<dyn EventStore>,
        engine: Arc<DispatchEngine>,
        max_attempts: u32,
        poll_interval: Duration,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            store,
            engine,
            max_attempts,
            poll_interval,
            shutdown_rx,
        }
    }

    /// Run the retry task continuously until a shutdown signal is received.
    /// This task polls the store periodically and retries failed deliveries.
    pub async fn run(mut self) {
        info!("Retry task starting");

        // If shutdown was already signaled before we started, exit immediately
        // rather than waiting a full poll interval to detect it.
        if *self.shutdown_rx.borrow() {
            info!("Retry task shutting down (pre-signaled)");
            return;
        }

        loop {
            tokio::select! {
                _ = tokio::time::sleep(self.poll_interval) => {
                    if let Err(e) = self.poll_and_retry().await {
                        error!("Error during retry poll: {}", e);
                    }
                }
                result = self.shutdown_rx.changed() => {
                    if result.is_err() || *self.shutdown_rx.borrow() {
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
            seq = delivery.seq,
            subscriber = delivery.subscriber_id,
            attempt = delivery.attempts,
            "retrying delivery"
        );

        // If we've already exhausted attempts, dead-letter immediately.
        // (query_retryable filters by attempts < max_attempts, so this is
        // a defensive check.)
        if delivery.attempts >= self.max_attempts {
            self.dead_letter(delivery, "max attempts exceeded").await;
            return;
        }

        // Attempt the retry via the engine
        let result = self
            .engine
            .retry_to_subscriber(
                &delivery.subscriber_id,
                &delivery.subject,
                &delivery.payload,
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
                    )
                    .await
                {
                    warn!(seq = delivery.seq, error = %e, "failed to record retry success");
                }
                // Re-evaluate the parent event's status now that one more
                // subscriber is delivered.
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
                // record_delivery increments attempts and computes next_retry
                // backoff via the store implementation.
                if let Err(e) = self
                    .store
                    .record_delivery(
                        delivery.seq,
                        &delivery.subscriber_id,
                        DeliveryStatus::Failed,
                        Some(msg.clone()),
                    )
                    .await
                {
                    warn!(seq = delivery.seq, error = %e, "failed to record retry failure");
                }
                // If this attempt was the last allowed one, dead-letter
                // immediately. Otherwise the entry would stop being returned
                // by query_retryable (which filters attempts < max_attempts)
                // and would be stuck in Failed state forever.
                if new_attempts >= self.max_attempts {
                    self.dead_letter(
                        delivery,
                        &format!("max attempts ({}) exhausted: {}", self.max_attempts, msg),
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
        if let Err(e) = self
            .store
            .record_delivery(
                delivery.seq,
                &delivery.subscriber_id,
                DeliveryStatus::DeadLettered,
                Some(format!("dead-lettered: {}", reason)),
            )
            .await
        {
            warn!(seq = delivery.seq, error = %e, "failed to record dead-letter");
        }
        self.maybe_finalize_event(delivery.seq).await;
    }

    /// If every delivery for an event is in a terminal state (Delivered or
    /// DeadLettered), update the event status accordingly. Sets the event
    /// to `DeadLettered` if any delivery is dead-lettered, otherwise
    /// `Delivered`.
    async fn maybe_finalize_event(&self, seq: u64) {
        // Read the event back from the store to inspect its delivery records.
        let events = match self
            .store
            .query_by_status(EventStatus::PartiallyDelivered, 10_000)
            .await
        {
            Ok(events) => events,
            Err(e) => {
                warn!(seq = seq, error = %e, "failed to query events for finalization");
                return;
            }
        };
        let event = match events.into_iter().find(|e| e.seq == seq) {
            Some(e) => e,
            None => return, // Already finalized or not found
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
