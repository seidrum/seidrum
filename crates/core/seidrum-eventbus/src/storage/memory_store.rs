use super::{
    DeliveryRecord, DeliveryStatus, EventStatus, EventStore, RetryableDelivery, StorageResult,
    StoredEvent,
};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;

/// An in-memory event store backed by a Vec. Useful for testing.
///
/// Sequence generation and insertion happen under the same write lock to
/// guarantee that events are stored in seq order even under concurrent appends.
pub struct InMemoryEventStore {
    /// Internal events storage. `pub(crate)` for test access to simulate time manipulation.
    pub(crate) events: Arc<RwLock<Vec<StoredEvent>>>,
}

impl InMemoryEventStore {
    pub fn new() -> Self {
        Self {
            events: Arc::new(RwLock::new(Vec::new())),
        }
    }

    fn current_time_ms() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

impl Default for InMemoryEventStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EventStore for InMemoryEventStore {
    async fn append(&self, event: &StoredEvent) -> StorageResult<u64> {
        // Acquire the write lock first, then derive seq from current length
        // so that seq generation and insertion are atomic.
        let mut events = self.events.write().await;
        let seq = events.last().map(|e| e.seq + 1).unwrap_or(1);
        let mut stored = event.clone();
        stored.seq = seq;
        stored.stored_at = Self::current_time_ms();
        events.push(stored);
        Ok(seq)
    }

    async fn get(&self, seq: u64) -> StorageResult<Option<StoredEvent>> {
        let events = self.events.read().await;
        Ok(events.iter().find(|e| e.seq == seq).cloned())
    }

    async fn update_status(&self, seq: u64, status: EventStatus) -> StorageResult<()> {
        let mut events = self.events.write().await;
        if let Some(event) = events.iter_mut().find(|e| e.seq == seq) {
            event.status = status;
            Ok(())
        } else {
            Err(super::StorageError::NotFound)
        }
    }

    async fn record_delivery(
        &self,
        seq: u64,
        subscriber_id: &str,
        status: DeliveryStatus,
        error: Option<String>,
        next_retry: Option<u64>,
    ) -> StorageResult<()> {
        let mut events = self.events.write().await;
        if let Some(event) = events.iter_mut().find(|e| e.seq == seq) {
            // Find existing delivery record or create a new one
            if let Some(delivery) = event
                .deliveries
                .iter_mut()
                .find(|d| d.subscriber_id == subscriber_id)
            {
                delivery.status = status;
                // Count attempts that represent a real delivery attempt
                // (Failed and DeadLettered) but not Delivered (which would
                // produce off-by-one observability).
                if matches!(
                    status,
                    DeliveryStatus::Failed | DeliveryStatus::DeadLettered
                ) {
                    delivery.attempts += 1;
                }
                delivery.last_attempt = Some(Self::current_time_ms());
                // Only overwrite the error on Failed/DeadLettered transitions.
                // Successful retries preserve the prior failure context for
                // diagnostics, regardless of what the caller passed.
                if matches!(
                    status,
                    DeliveryStatus::Failed | DeliveryStatus::DeadLettered
                ) {
                    delivery.error = error;
                }
                delivery.next_retry = next_retry;
            } else {
                let now = Self::current_time_ms();
                event.deliveries.push(DeliveryRecord {
                    subscriber_id: subscriber_id.to_string(),
                    status,
                    attempts: if status == DeliveryStatus::Failed {
                        1
                    } else {
                        0
                    },
                    last_attempt: Some(now),
                    next_retry,
                    error,
                });
            }
            Ok(())
        } else {
            Err(super::StorageError::NotFound)
        }
    }

    async fn query_by_status(
        &self,
        status: EventStatus,
        limit: usize,
    ) -> StorageResult<Vec<StoredEvent>> {
        let events = self.events.read().await;
        Ok(events
            .iter()
            .filter(|e| e.status == status)
            .take(limit)
            .cloned()
            .collect())
    }

    async fn query_by_subject(
        &self,
        subject: &str,
        since: Option<u64>,
        limit: usize,
    ) -> StorageResult<Vec<StoredEvent>> {
        let events = self.events.read().await;
        Ok(events
            .iter()
            .filter(|e| e.subject == subject && since.is_none_or(|s| e.seq >= s))
            .take(limit)
            .cloned()
            .collect())
    }

    async fn query_retryable(
        &self,
        max_attempts: u32,
        limit: usize,
    ) -> StorageResult<Vec<RetryableDelivery>> {
        let events = self.events.read().await;
        let mut retryable = Vec::new();

        let now = Self::current_time_ms();
        for event in events.iter() {
            for delivery in &event.deliveries {
                if delivery.is_retryable(max_attempts, now) {
                    retryable.push(RetryableDelivery {
                        seq: event.seq,
                        subject: event.subject.clone(),
                        subscriber_id: delivery.subscriber_id.clone(),
                        attempts: delivery.attempts,
                        payload: event.payload.clone(),
                        reply_subject: event.reply_subject.clone(),
                        next_retry: delivery.next_retry,
                    });
                }
            }
        }

        // Sort by next_retry ascending so the earliest-due deliveries come
        // first. None (no retry scheduled) sorts first.
        retryable.sort_by_key(|r| r.next_retry.unwrap_or(0));
        Ok(retryable.into_iter().take(limit).collect())
    }

    async fn count_retryable(&self, max_attempts: u32) -> StorageResult<u64> {
        let events = self.events.read().await;
        let now = Self::current_time_ms();
        let mut count = 0u64;
        for event in events.iter() {
            for delivery in &event.deliveries {
                if delivery.is_retryable(max_attempts, now) {
                    count += 1;
                }
            }
        }
        Ok(count)
    }

    async fn compact(&self, older_than: Duration) -> StorageResult<u64> {
        let now = Self::current_time_ms();
        let threshold = now.saturating_sub(older_than.as_millis() as u64);

        let mut events = self.events.write().await;
        let original_len = events.len();
        events.retain(|e| {
            // Keep events that are not in a terminal state, or are newer than threshold.
            // Terminal states (Delivered, DeadLettered) are eligible for compaction.
            let terminal = matches!(e.status, EventStatus::Delivered | EventStatus::DeadLettered);
            !(terminal && e.stored_at < threshold)
        });

        Ok((original_len - events.len()) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_memory_store_append() {
        let store = InMemoryEventStore::new();
        let event = StoredEvent {
            seq: 0,
            subject: "test".to_string(),
            payload: b"data".to_vec(),
            stored_at: 1000,
            status: EventStatus::Pending,
            deliveries: vec![],
            reply_subject: None,
        };

        let seq = store.append(&event).await.unwrap();
        assert!(seq > 0);
    }

    #[tokio::test]
    async fn test_memory_store_concurrent_appends() {
        let store = Arc::new(InMemoryEventStore::new());
        let mut handles = vec![];

        for i in 0..10 {
            let store_clone = Arc::clone(&store);
            let handle = tokio::spawn(async move {
                let event = StoredEvent {
                    seq: 0,
                    subject: format!("test.{}", i),
                    payload: vec![i as u8],
                    stored_at: 1000 + i as u64,
                    status: EventStatus::Pending,
                    deliveries: vec![],
                    reply_subject: None,
                };
                store_clone.append(&event).await.unwrap()
            });
            handles.push(handle);
        }

        let mut seqs = vec![];
        for handle in handles {
            seqs.push(handle.await.unwrap());
        }

        // All sequences should be unique and ordered
        seqs.sort();
        for i in 0..seqs.len() - 1 {
            assert!(seqs[i] < seqs[i + 1]);
        }
    }
}
