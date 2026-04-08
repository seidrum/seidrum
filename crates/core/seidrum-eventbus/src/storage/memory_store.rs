use super::{
    DeliveryRecord, DeliveryStatus, EventStatus, EventStore, RetryableDelivery, StorageResult,
    StoredEvent,
};
use async_trait::async_trait;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;

/// An in-memory event store backed by a Vec. Useful for testing.
pub struct InMemoryEventStore {
    /// Internal events storage. `pub(crate)` for test access to simulate time manipulation.
    pub(crate) events: Arc<RwLock<Vec<StoredEvent>>>,
    next_seq: AtomicU64,
}

impl InMemoryEventStore {
    pub fn new() -> Self {
        Self {
            events: Arc::new(RwLock::new(Vec::new())),
            next_seq: AtomicU64::new(1),
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
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let mut stored = event.clone();
        stored.seq = seq;
        stored.stored_at = Self::current_time_ms();

        let mut events = self.events.write().await;
        events.push(stored);

        Ok(seq)
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
                delivery.attempts += 1;
                delivery.last_attempt = Some(Self::current_time_ms());
            } else {
                event.deliveries.push(DeliveryRecord {
                    subscriber_id: subscriber_id.to_string(),
                    status,
                    attempts: 1,
                    last_attempt: Some(Self::current_time_ms()),
                    next_retry: None,
                    error: None,
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

        for event in events.iter() {
            for delivery in &event.deliveries {
                if delivery.status == DeliveryStatus::Failed && delivery.attempts < max_attempts {
                    retryable.push(RetryableDelivery {
                        seq: event.seq,
                        subject: event.subject.clone(),
                        subscriber_id: delivery.subscriber_id.clone(),
                        attempts: delivery.attempts,
                        payload: event.payload.clone(),
                    });
                }
            }
        }

        Ok(retryable.into_iter().take(limit).collect())
    }

    async fn compact(&self, older_than: Duration) -> StorageResult<u64> {
        let now = Self::current_time_ms();
        let threshold = now - older_than.as_millis() as u64;

        let mut events = self.events.write().await;
        let original_len = events.len();
        events.retain(|e| {
            // Keep events that are not fully delivered, or are newer than threshold
            !(e.status == EventStatus::Delivered && e.stored_at < threshold)
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
