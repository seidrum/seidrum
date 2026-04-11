//! Storage layer for the event bus.
//!
//! Defines the [`EventStore`] trait and provides two implementations:
//! - [`memory_store::InMemoryEventStore`] — `Vec`-backed, no persistence,
//!   useful for tests.
//! - [`redb_store::RedbEventStore`] — durable on-disk store backed by
//!   [`redb`](https://docs.rs/redb).
//!
//! All events flow through `append()` (write-ahead) before dispatch. The
//! [`compaction::CompactionTask`] periodically removes events in terminal
//! states (`Delivered`, `DeadLettered`) older than the retention threshold.

mod types;
pub use types::*;

pub mod compaction;
pub mod memory_store;
pub mod redb_store;

use async_trait::async_trait;
use std::time::Duration;
use thiserror::Error;

/// Errors returned by [`EventStore`] implementations.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("storage operation failed: {0}")]
    OperationFailed(String),
    #[error("event not found")]
    NotFound,
    #[error("database error: {0}")]
    DatabaseError(String),
}

/// Convenience alias for [`EventStore`] return values.
pub type StorageResult<T> = Result<T, StorageError>;

/// The `EventStore` trait defines how events are persisted and queried.
///
/// Implementations must be `Send + Sync + 'static` so the bus can share
/// them across tasks via `Arc<dyn EventStore>`.
#[async_trait]
pub trait EventStore: Send + Sync + 'static {
    /// Persist an event. Returns the assigned sequence number.
    /// This is the write-ahead step — the event is durable after this call returns.
    async fn append(&self, event: &StoredEvent) -> StorageResult<u64>;

    /// Fetch a single event by its sequence number.
    /// Returns `Ok(None)` if no event with that seq exists.
    async fn get(&self, seq: u64) -> StorageResult<Option<StoredEvent>>;

    /// Update the dispatch status of an event.
    ///
    /// Implementations should defensively clean any stale per-subscriber
    /// retry-tracking metadata when transitioning to a terminal state
    /// (`Delivered` or `DeadLettered`).
    async fn update_status(&self, seq: u64, status: EventStatus) -> StorageResult<()>;

    /// Record delivery outcome for one subscriber.
    ///
    /// - `error`: optional human-readable error message. Preserved on success
    ///   transitions so the last failure context isn't wiped.
    /// - `next_retry`: optional unix-millis timestamp indicating when the
    ///   delivery is eligible for the next retry attempt. Callers compute
    ///   this using the canonical [`crate::delivery::calculate_backoff`]
    ///   helper. Stored verbatim by the implementation.
    ///
    /// `attempts` is incremented only on `Failed` transitions; success and
    /// dead-letter transitions preserve the existing count for accurate
    /// observability.
    async fn record_delivery(
        &self,
        seq: u64,
        subscriber_id: &str,
        status: DeliveryStatus,
        error: Option<String>,
        next_retry: Option<u64>,
    ) -> StorageResult<()>;

    /// Query events by status (for crash recovery and retry).
    async fn query_by_status(
        &self,
        status: EventStatus,
        limit: usize,
    ) -> StorageResult<Vec<StoredEvent>>;

    /// Query events by subject pattern (for replay and debugging).
    /// Pattern matching is exact-only in Phase 1.
    async fn query_by_subject(
        &self,
        subject: &str,
        since: Option<u64>,
        limit: usize,
    ) -> StorageResult<Vec<StoredEvent>>;

    /// Query failed deliveries that are due for retry.
    ///
    /// Returns deliveries whose `next_retry` is `<= now` (or unset),
    /// ordered ascending by `next_retry` so the earliest-due deliveries
    /// are processed first. This prevents older deliveries from starving
    /// newer urgent ones.
    async fn query_retryable(
        &self,
        max_attempts: u32,
        limit: usize,
    ) -> StorageResult<Vec<RetryableDelivery>>;

    /// Count failed deliveries eligible for retry (cheap version of
    /// [`Self::query_retryable`] for metrics scraping).
    ///
    /// Default implementation calls `query_retryable` with `usize::MAX`
    /// and counts. Implementations should override with an O(1) or
    /// O(failed events) version where possible.
    async fn count_retryable(&self, max_attempts: u32) -> StorageResult<u64> {
        let v = self.query_retryable(max_attempts, usize::MAX).await?;
        Ok(v.len() as u64)
    }

    /// Compact: remove fully-delivered events older than the retention threshold.
    /// Returns the number of events removed.
    async fn compact(&self, older_than: Duration) -> StorageResult<u64>;

    /// Query dead-lettered events for inspection or replay.
    ///
    /// Convenience wrapper around `query_by_status(EventStatus::DeadLettered, limit)`.
    async fn query_dead_lettered(&self, limit: usize) -> StorageResult<Vec<StoredEvent>> {
        self.query_by_status(EventStatus::DeadLettered, limit).await
    }

    // === Persisted subscriptions (Phase 4) ===
    //
    // Webhook subscriptions registered via the HTTP transport are persisted
    // here so they survive restarts. In-process subscriptions are not
    // persisted — only durable transports (webhook) need this.
    //
    // The default implementations return `StorageError::OperationFailed`
    // (NOT `unimplemented!`) so a downstream `EventStore` that predates
    // these methods compiles unchanged AND keeps running cleanly: writes
    // surface as 5xx responses from the HTTP transport, and `start()`
    // tolerates a missing `list_subscriptions` by treating it as "no
    // persisted subs" rather than panicking the server task.

    /// Save a persisted subscription. Replaces any existing entry with the
    /// same `persisted_id`.
    async fn save_subscription(&self, sub: &PersistedSubscription) -> StorageResult<()> {
        let _ = sub;
        Err(StorageError::OperationFailed(
            "subscription persistence not supported by this store".to_string(),
        ))
    }

    /// List all persisted subscriptions. Used on HTTP server startup to
    /// recreate webhook subscriptions after a restart.
    ///
    /// Default returns an empty list so a store without persistence
    /// support starts cleanly with zero recreated subscriptions, rather
    /// than panicking the HTTP server task.
    async fn list_subscriptions(&self) -> StorageResult<Vec<PersistedSubscription>> {
        Ok(Vec::new())
    }

    /// Delete a persisted subscription by its `persisted_id`.
    async fn delete_subscription(&self, persisted_id: &str) -> StorageResult<()> {
        let _ = persisted_id;
        Err(StorageError::OperationFailed(
            "subscription persistence not supported by this store".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::memory_store::InMemoryEventStore;

    #[tokio::test]
    async fn test_append_and_query() {
        let store = InMemoryEventStore::new();
        let event = StoredEvent {
            seq: 0,
            subject: "test.subject".to_string(),
            payload: b"test payload".to_vec(),
            stored_at: 1000,
            status: EventStatus::Pending,
            deliveries: vec![],
            reply_subject: None,
        };

        let seq = store.append(&event).await.unwrap();
        assert_eq!(seq, 1);

        let results = store
            .query_by_subject("test.subject", None, 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].payload, b"test payload");
    }

    #[tokio::test]
    async fn test_seq_monotonic() {
        let store = InMemoryEventStore::new();
        let event1 = StoredEvent {
            seq: 0,
            subject: "test".to_string(),
            payload: b"1".to_vec(),
            stored_at: 1000,
            status: EventStatus::Pending,
            deliveries: vec![],
            reply_subject: None,
        };
        let event2 = StoredEvent {
            seq: 0,
            subject: "test".to_string(),
            payload: b"2".to_vec(),
            stored_at: 2000,
            status: EventStatus::Pending,
            deliveries: vec![],
            reply_subject: None,
        };

        let seq1 = store.append(&event1).await.unwrap();
        let seq2 = store.append(&event2).await.unwrap();
        assert!(seq2 > seq1);
    }

    #[tokio::test]
    async fn test_status_update() {
        let store = InMemoryEventStore::new();
        let event = StoredEvent {
            seq: 0,
            subject: "test".to_string(),
            payload: vec![],
            stored_at: 1000,
            status: EventStatus::Pending,
            deliveries: vec![],
            reply_subject: None,
        };

        let seq = store.append(&event).await.unwrap();
        store
            .update_status(seq, EventStatus::Delivered)
            .await
            .unwrap();

        let results = store
            .query_by_status(EventStatus::Delivered, 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, EventStatus::Delivered);
    }

    #[tokio::test]
    async fn test_delivery_recording() {
        let store = InMemoryEventStore::new();
        let event = StoredEvent {
            seq: 0,
            subject: "test".to_string(),
            payload: vec![],
            stored_at: 1000,
            status: EventStatus::Pending,
            deliveries: vec![],
            reply_subject: None,
        };

        let seq = store.append(&event).await.unwrap();
        store
            .record_delivery(seq, "sub1", DeliveryStatus::Delivered, None, None)
            .await
            .unwrap();

        let results = store.query_by_subject("test", None, 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].deliveries.len(), 1);
        assert_eq!(results[0].deliveries[0].subscriber_id, "sub1");
    }

    #[tokio::test]
    async fn test_query_by_status_filter() {
        let store = InMemoryEventStore::new();

        let event1 = StoredEvent {
            seq: 0,
            subject: "test".to_string(),
            payload: b"1".to_vec(),
            stored_at: 1000,
            status: EventStatus::Pending,
            deliveries: vec![],
            reply_subject: None,
        };
        let event2 = StoredEvent {
            seq: 0,
            subject: "test".to_string(),
            payload: b"2".to_vec(),
            stored_at: 2000,
            status: EventStatus::Delivered,
            deliveries: vec![],
            reply_subject: None,
        };

        store.append(&event1).await.unwrap();
        store.append(&event2).await.unwrap();

        let pending = store
            .query_by_status(EventStatus::Pending, 10)
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);

        let delivered = store
            .query_by_status(EventStatus::Delivered, 10)
            .await
            .unwrap();
        assert_eq!(delivered.len(), 1);
    }

    #[tokio::test]
    async fn test_query_by_subject_exact() {
        let store = InMemoryEventStore::new();

        let event1 = StoredEvent {
            seq: 0,
            subject: "test.a".to_string(),
            payload: b"1".to_vec(),
            stored_at: 1000,
            status: EventStatus::Pending,
            deliveries: vec![],
            reply_subject: None,
        };
        let event2 = StoredEvent {
            seq: 0,
            subject: "test.b".to_string(),
            payload: b"2".to_vec(),
            stored_at: 2000,
            status: EventStatus::Pending,
            deliveries: vec![],
            reply_subject: None,
        };

        store.append(&event1).await.unwrap();
        store.append(&event2).await.unwrap();

        let results = store.query_by_subject("test.a", None, 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].payload, b"1");
    }

    #[tokio::test]
    async fn test_compaction_delivered() {
        let store = InMemoryEventStore::new();

        let old_event = StoredEvent {
            seq: 0,
            subject: "test".to_string(),
            payload: b"old".to_vec(),
            stored_at: 0,
            status: EventStatus::Delivered,
            deliveries: vec![],
            reply_subject: None,
        };
        let new_event = StoredEvent {
            seq: 0,
            subject: "test".to_string(),
            payload: b"new".to_vec(),
            stored_at: 0,
            status: EventStatus::Delivered,
            deliveries: vec![],
            reply_subject: None,
        };

        // Both events get stored_at = now from the store's append().
        let old_seq = store.append(&old_event).await.unwrap();
        store.append(&new_event).await.unwrap();

        // Manually backdate the old event's stored_at to simulate age.
        {
            let mut events = store.events.write().await;
            if let Some(e) = events.iter_mut().find(|e| e.seq == old_seq) {
                e.stored_at = 1000; // ancient timestamp
            }
        }

        // Compact events older than 5 seconds (5000ms)
        let removed = store.compact(Duration::from_millis(5000)).await.unwrap();
        assert_eq!(removed, 1);

        let remaining = store.query_by_subject("test", None, 10).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].payload, b"new");
    }
}
