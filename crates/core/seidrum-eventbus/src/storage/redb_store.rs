use super::{
    DeliveryRecord, DeliveryStatus, EventStatus, EventStore, RetryableDelivery, StorageError,
    StorageResult, StoredEvent,
};
use async_trait::async_trait;
use redb::{Database, ReadableTable};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Table definitions for redb
const EVENTS_TABLE: redb::TableDefinition<u64, &[u8]> = redb::TableDefinition::new("events");
const SUBJECT_IDX_TABLE: redb::TableDefinition<(&str, u64), ()> =
    redb::TableDefinition::new("subject_idx");
const STATUS_IDX_TABLE: redb::TableDefinition<(u8, u64), ()> =
    redb::TableDefinition::new("status_idx");

/// A durable event store using redb as the backing storage engine.
pub struct RedbEventStore {
    db: Arc<Database>,
}

impl RedbEventStore {
    /// Open or create a redb-backed event store at the given path.
    pub fn open<P: AsRef<Path>>(path: P) -> StorageResult<Self> {
        let db = Database::create(path)
            .map_err(|e| StorageError::DatabaseError(format!("failed to open redb: {}", e)))?;

        // Ensure tables exist
        {
            let write_txn = db.begin_write().map_err(|e| {
                StorageError::DatabaseError(format!("failed to begin transaction: {}", e))
            })?;
            {
                let _t1 = write_txn.open_table(EVENTS_TABLE).map_err(|e| {
                    StorageError::DatabaseError(format!("failed to open events table: {}", e))
                })?;
                let _t2 = write_txn.open_table(SUBJECT_IDX_TABLE).map_err(|e| {
                    StorageError::DatabaseError(format!("failed to open subject_idx table: {}", e))
                })?;
                let _t3 = write_txn.open_table(STATUS_IDX_TABLE).map_err(|e| {
                    StorageError::DatabaseError(format!("failed to open status_idx table: {}", e))
                })?;
            }
            write_txn.commit().map_err(|e| {
                StorageError::DatabaseError(format!("failed to commit transaction: {}", e))
            })?;
        }

        Ok(Self { db: Arc::new(db) })
    }

    fn current_time_ms() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

#[async_trait]
impl EventStore for RedbEventStore {
    async fn append(&self, event: &StoredEvent) -> StorageResult<u64> {
        let db = Arc::clone(&self.db);
        let event = event.clone();

        tokio::task::spawn_blocking(move || {
            // Use a single write transaction for atomic seq generation + insert.
            // This prevents race conditions where two concurrent appends could
            // read the same last_seq from separate read transactions.
            let write_txn = db.begin_write().map_err(|e| {
                StorageError::DatabaseError(format!("failed to begin write: {}", e))
            })?;

            // Get next seq inside the write transaction (serialized by redb)
            let next_seq = {
                let events_table = write_txn.open_table(EVENTS_TABLE).map_err(|e| {
                    StorageError::DatabaseError(format!("failed to open events table: {}", e))
                })?;
                let last_seq: u64 = events_table
                    .last()
                    .map_err(|e| StorageError::DatabaseError(format!("failed to get last: {}", e)))?
                    .map(|(k, _)| k.value())
                    .unwrap_or(0);
                last_seq + 1
            };

            let mut stored = event;
            stored.seq = next_seq;
            stored.stored_at = RedbEventStore::current_time_ms();

            let serialized = serde_json::to_vec(&stored).map_err(|e| {
                StorageError::OperationFailed(format!("failed to serialize event: {}", e))
            })?;

            {
                let mut events_table = write_txn.open_table(EVENTS_TABLE).map_err(|e| {
                    StorageError::DatabaseError(format!("failed to open events table: {}", e))
                })?;
                events_table
                    .insert(next_seq, serialized.as_slice())
                    .map_err(|e| {
                        StorageError::DatabaseError(format!("failed to insert event: {}", e))
                    })?;
            }
            {
                let mut subject_idx = write_txn.open_table(SUBJECT_IDX_TABLE).map_err(|e| {
                    StorageError::DatabaseError(format!("failed to open subject_idx: {}", e))
                })?;
                subject_idx
                    .insert((stored.subject.as_str(), next_seq), ())
                    .map_err(|e| {
                        StorageError::DatabaseError(format!(
                            "failed to insert subject index: {}",
                            e
                        ))
                    })?;
            }
            {
                let mut status_idx = write_txn.open_table(STATUS_IDX_TABLE).map_err(|e| {
                    StorageError::DatabaseError(format!("failed to open status_idx: {}", e))
                })?;
                status_idx
                    .insert((stored.status.as_u8(), next_seq), ())
                    .map_err(|e| {
                        StorageError::DatabaseError(format!("failed to insert status index: {}", e))
                    })?;
            }
            write_txn
                .commit()
                .map_err(|e| StorageError::DatabaseError(format!("failed to commit: {}", e)))?;

            Ok(next_seq)
        })
        .await
        .map_err(|e| StorageError::OperationFailed(format!("spawn_blocking error: {}", e)))?
    }

    async fn update_status(&self, seq: u64, status: EventStatus) -> StorageResult<()> {
        let db = Arc::clone(&self.db);

        tokio::task::spawn_blocking(move || {
            let write_txn = db.begin_write().map_err(|e| {
                StorageError::DatabaseError(format!("failed to begin write: {}", e))
            })?;

            // Read the event
            let old_status_u8;
            let serialized = {
                let events_table = write_txn.open_table(EVENTS_TABLE).map_err(|e| {
                    StorageError::DatabaseError(format!("failed to open events table: {}", e))
                })?;
                let data = events_table
                    .get(seq)
                    .map_err(|e| {
                        StorageError::DatabaseError(format!("failed to get event: {}", e))
                    })?
                    .ok_or(StorageError::NotFound)?;
                let bytes = data.value().to_vec();
                let mut event: StoredEvent = serde_json::from_slice(&bytes).map_err(|e| {
                    StorageError::OperationFailed(format!("failed to deserialize event: {}", e))
                })?;
                old_status_u8 = event.status.as_u8();
                event.status = status;
                serde_json::to_vec(&event).map_err(|e| {
                    StorageError::OperationFailed(format!("failed to serialize event: {}", e))
                })?
            };

            // Write back
            {
                let mut events_table = write_txn.open_table(EVENTS_TABLE).map_err(|e| {
                    StorageError::DatabaseError(format!("failed to open events table: {}", e))
                })?;
                events_table
                    .insert(seq, serialized.as_slice())
                    .map_err(|e| {
                        StorageError::DatabaseError(format!("failed to update event: {}", e))
                    })?;
            }

            // Update status index: remove old, insert new
            {
                let mut status_idx = write_txn.open_table(STATUS_IDX_TABLE).map_err(|e| {
                    StorageError::DatabaseError(format!("failed to open status_idx: {}", e))
                })?;
                let _ = status_idx.remove((old_status_u8, seq));
                status_idx.insert((status.as_u8(), seq), ()).map_err(|e| {
                    StorageError::DatabaseError(format!("failed to insert new status index: {}", e))
                })?;
            }

            write_txn
                .commit()
                .map_err(|e| StorageError::DatabaseError(format!("failed to commit: {}", e)))
        })
        .await
        .map_err(|e| StorageError::OperationFailed(format!("spawn_blocking error: {}", e)))?
    }

    async fn record_delivery(
        &self,
        seq: u64,
        subscriber_id: &str,
        status: DeliveryStatus,
    ) -> StorageResult<()> {
        let db = Arc::clone(&self.db);
        let subscriber_id = subscriber_id.to_string();

        tokio::task::spawn_blocking(move || {
            let write_txn = db.begin_write().map_err(|e| {
                StorageError::DatabaseError(format!("failed to begin write: {}", e))
            })?;

            // Read the event
            let serialized = {
                let events_table = write_txn.open_table(EVENTS_TABLE).map_err(|e| {
                    StorageError::DatabaseError(format!("failed to open events table: {}", e))
                })?;
                let data = events_table
                    .get(seq)
                    .map_err(|e| {
                        StorageError::DatabaseError(format!("failed to get event: {}", e))
                    })?
                    .ok_or(StorageError::NotFound)?;
                let bytes = data.value().to_vec();
                let mut event: StoredEvent = serde_json::from_slice(&bytes).map_err(|e| {
                    StorageError::OperationFailed(format!("failed to deserialize event: {}", e))
                })?;

                // Update or insert delivery record
                if let Some(delivery) = event
                    .deliveries
                    .iter_mut()
                    .find(|d| d.subscriber_id == subscriber_id)
                {
                    delivery.status = status;
                    delivery.attempts += 1;
                    delivery.last_attempt = Some(RedbEventStore::current_time_ms());
                } else {
                    event.deliveries.push(DeliveryRecord {
                        subscriber_id,
                        status,
                        attempts: 1,
                        last_attempt: Some(RedbEventStore::current_time_ms()),
                        next_retry: None,
                        error: None,
                    });
                }

                serde_json::to_vec(&event).map_err(|e| {
                    StorageError::OperationFailed(format!("failed to serialize event: {}", e))
                })?
            };

            // Write back
            {
                let mut events_table = write_txn.open_table(EVENTS_TABLE).map_err(|e| {
                    StorageError::DatabaseError(format!("failed to open events table: {}", e))
                })?;
                events_table
                    .insert(seq, serialized.as_slice())
                    .map_err(|e| {
                        StorageError::DatabaseError(format!("failed to update event: {}", e))
                    })?;
            }

            write_txn
                .commit()
                .map_err(|e| StorageError::DatabaseError(format!("failed to commit: {}", e)))
        })
        .await
        .map_err(|e| StorageError::OperationFailed(format!("spawn_blocking error: {}", e)))?
    }

    async fn query_by_status(
        &self,
        status: EventStatus,
        limit: usize,
    ) -> StorageResult<Vec<StoredEvent>> {
        let db = Arc::clone(&self.db);
        let status_u8 = status.as_u8();

        tokio::task::spawn_blocking(move || {
            let read_txn = db
                .begin_read()
                .map_err(|e| StorageError::DatabaseError(format!("failed to begin read: {}", e)))?;
            let status_idx = read_txn.open_table(STATUS_IDX_TABLE).map_err(|e| {
                StorageError::DatabaseError(format!("failed to open status_idx: {}", e))
            })?;
            let events_table = read_txn.open_table(EVENTS_TABLE).map_err(|e| {
                StorageError::DatabaseError(format!("failed to open events table: {}", e))
            })?;

            let mut results = Vec::new();
            let iter = status_idx.range((status_u8, u64::MIN)..).map_err(|e| {
                StorageError::DatabaseError(format!("failed to range query: {}", e))
            })?;

            for item in iter {
                let (key, _) = item.map_err(|e| {
                    StorageError::DatabaseError(format!("failed to iterate: {}", e))
                })?;
                let (key_status, seq): (u8, u64) = key.value();
                if key_status != status_u8 {
                    break;
                }
                if results.len() >= limit {
                    break;
                }

                if let Some(data) = events_table.get(seq).map_err(|e| {
                    StorageError::DatabaseError(format!("failed to get event: {}", e))
                })? {
                    let bytes = data.value().to_vec();
                    let event: StoredEvent = serde_json::from_slice(&bytes).map_err(|e| {
                        StorageError::OperationFailed(format!("failed to deserialize: {}", e))
                    })?;
                    results.push(event);
                }
            }

            Ok(results)
        })
        .await
        .map_err(|e| StorageError::OperationFailed(format!("spawn_blocking error: {}", e)))?
    }

    async fn query_by_subject(
        &self,
        subject: &str,
        since: Option<u64>,
        limit: usize,
    ) -> StorageResult<Vec<StoredEvent>> {
        let db = Arc::clone(&self.db);
        let subject = subject.to_string();

        tokio::task::spawn_blocking(move || {
            let read_txn = db
                .begin_read()
                .map_err(|e| StorageError::DatabaseError(format!("failed to begin read: {}", e)))?;
            let subject_idx = read_txn.open_table(SUBJECT_IDX_TABLE).map_err(|e| {
                StorageError::DatabaseError(format!("failed to open subject_idx: {}", e))
            })?;
            let events_table = read_txn.open_table(EVENTS_TABLE).map_err(|e| {
                StorageError::DatabaseError(format!("failed to open events table: {}", e))
            })?;

            let mut results = Vec::new();
            let start_seq = since.unwrap_or(0);
            let iter = subject_idx
                .range((subject.as_str(), start_seq)..)
                .map_err(|e| {
                    StorageError::DatabaseError(format!("failed to range query: {}", e))
                })?;

            for item in iter {
                let (key, _) = item.map_err(|e| {
                    StorageError::DatabaseError(format!("failed to iterate: {}", e))
                })?;
                let (key_subject, seq): (&str, u64) = key.value();
                if key_subject != subject.as_str() {
                    break;
                }
                if results.len() >= limit {
                    break;
                }

                if let Some(data) = events_table.get(seq).map_err(|e| {
                    StorageError::DatabaseError(format!("failed to get event: {}", e))
                })? {
                    let bytes = data.value().to_vec();
                    let event: StoredEvent = serde_json::from_slice(&bytes).map_err(|e| {
                        StorageError::OperationFailed(format!("failed to deserialize: {}", e))
                    })?;
                    results.push(event);
                }
            }

            Ok(results)
        })
        .await
        .map_err(|e| StorageError::OperationFailed(format!("spawn_blocking error: {}", e)))?
    }

    async fn query_retryable(
        &self,
        max_attempts: u32,
        limit: usize,
    ) -> StorageResult<Vec<RetryableDelivery>> {
        let db = Arc::clone(&self.db);

        // TODO(Phase 2): Add a delivery_idx table keyed by (subscriber_id, seq) → DeliveryStatus
        // to avoid this full-table scan. Currently O(n) over all events.
        tokio::task::spawn_blocking(move || {
            let read_txn = db
                .begin_read()
                .map_err(|e| StorageError::DatabaseError(format!("failed to begin read: {}", e)))?;
            let events_table = read_txn.open_table(EVENTS_TABLE).map_err(|e| {
                StorageError::DatabaseError(format!("failed to open events table: {}", e))
            })?;

            let mut results = Vec::new();
            let iter = events_table
                .iter()
                .map_err(|e| StorageError::DatabaseError(format!("failed to iterate: {}", e)))?;

            for item in iter {
                let (_, data) = item.map_err(|e| {
                    StorageError::DatabaseError(format!("failed to iterate: {}", e))
                })?;
                if results.len() >= limit {
                    break;
                }

                let bytes = data.value().to_vec();
                let event: StoredEvent = serde_json::from_slice(&bytes).map_err(|e| {
                    StorageError::OperationFailed(format!("failed to deserialize: {}", e))
                })?;

                for delivery in &event.deliveries {
                    if results.len() >= limit {
                        break;
                    }
                    if delivery.status == DeliveryStatus::Failed && delivery.attempts < max_attempts
                    {
                        results.push(RetryableDelivery {
                            seq: event.seq,
                            subject: event.subject.clone(),
                            subscriber_id: delivery.subscriber_id.clone(),
                            attempts: delivery.attempts,
                            payload: event.payload.clone(),
                        });
                    }
                }
            }

            Ok(results)
        })
        .await
        .map_err(|e| StorageError::OperationFailed(format!("spawn_blocking error: {}", e)))?
    }

    async fn compact(&self, older_than: Duration) -> StorageResult<u64> {
        let db = Arc::clone(&self.db);
        let threshold =
            RedbEventStore::current_time_ms().saturating_sub(older_than.as_millis() as u64);

        tokio::task::spawn_blocking(move || {
            // First pass: read and collect items to delete
            let to_delete: Vec<(u64, String, u8)> = {
                let read_txn = db.begin_read().map_err(|e| {
                    StorageError::DatabaseError(format!("failed to begin read: {}", e))
                })?;
                let events_table = read_txn.open_table(EVENTS_TABLE).map_err(|e| {
                    StorageError::DatabaseError(format!("failed to open events table: {}", e))
                })?;

                let mut items = Vec::new();
                let iter = events_table.iter().map_err(|e| {
                    StorageError::DatabaseError(format!("failed to iterate: {}", e))
                })?;

                for item in iter {
                    let (key, data) = item.map_err(|e| {
                        StorageError::DatabaseError(format!("failed to iterate: {}", e))
                    })?;
                    let seq: u64 = key.value();
                    let bytes = data.value().to_vec();
                    let event: StoredEvent = serde_json::from_slice(&bytes).map_err(|e| {
                        StorageError::OperationFailed(format!("failed to deserialize: {}", e))
                    })?;

                    if event.status == EventStatus::Delivered && event.stored_at < threshold {
                        items.push((seq, event.subject, event.status.as_u8()));
                    }
                }
                items
            };

            if to_delete.is_empty() {
                return Ok(0);
            }

            // Second pass: delete collected items
            let write_txn = db.begin_write().map_err(|e| {
                StorageError::DatabaseError(format!("failed to begin write: {}", e))
            })?;

            let removed = to_delete.len() as u64;

            for (seq, subject, status_u8) in &to_delete {
                {
                    let mut events_table = write_txn.open_table(EVENTS_TABLE).map_err(|e| {
                        StorageError::DatabaseError(format!("failed to open events table: {}", e))
                    })?;
                    let _ = events_table.remove(*seq);
                }
                {
                    let mut subject_idx = write_txn.open_table(SUBJECT_IDX_TABLE).map_err(|e| {
                        StorageError::DatabaseError(format!("failed to open subject_idx: {}", e))
                    })?;
                    let _ = subject_idx.remove((subject.as_str(), *seq));
                }
                {
                    let mut status_idx = write_txn.open_table(STATUS_IDX_TABLE).map_err(|e| {
                        StorageError::DatabaseError(format!("failed to open status_idx: {}", e))
                    })?;
                    let _ = status_idx.remove((*status_u8, *seq));
                }
            }

            write_txn
                .commit()
                .map_err(|e| StorageError::DatabaseError(format!("failed to commit: {}", e)))?;

            Ok(removed)
        })
        .await
        .map_err(|e| StorageError::OperationFailed(format!("spawn_blocking error: {}", e)))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_redb_append_and_query() {
        let temp = TempDir::new().unwrap();
        let store = RedbEventStore::open(temp.path().join("events.db")).unwrap();

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
        assert!(seq > 0);

        let results = store
            .query_by_subject("test.subject", None, 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].payload, b"test payload");
    }

    #[tokio::test]
    async fn test_redb_seq_monotonic() {
        let temp = TempDir::new().unwrap();
        let store = RedbEventStore::open(temp.path().join("events.db")).unwrap();

        let mut seqs = Vec::new();
        for i in 0..5 {
            let event = StoredEvent {
                seq: 0,
                subject: "test".to_string(),
                payload: vec![i],
                stored_at: 1000,
                status: EventStatus::Pending,
                deliveries: vec![],
                reply_subject: None,
            };
            seqs.push(store.append(&event).await.unwrap());
        }

        for i in 0..seqs.len() - 1 {
            assert!(seqs[i] < seqs[i + 1]);
        }
    }

    #[tokio::test]
    async fn test_redb_status_update() {
        let temp = TempDir::new().unwrap();
        let store = RedbEventStore::open(temp.path().join("events.db")).unwrap();

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
    async fn test_redb_crash_recovery() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("events.db");

        // Write some events
        {
            let store = RedbEventStore::open(&path).unwrap();
            let event = StoredEvent {
                seq: 0,
                subject: "test".to_string(),
                payload: b"data".to_vec(),
                stored_at: 1000,
                status: EventStatus::Pending,
                deliveries: vec![],
                reply_subject: None,
            };
            let _seq = store.append(&event).await.unwrap();
        }

        // Reopen and verify the event is still there
        {
            let store = RedbEventStore::open(&path).unwrap();
            let results = store.query_by_subject("test", None, 10).await.unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].status, EventStatus::Pending);
        }
    }

    #[tokio::test]
    async fn test_redb_delivery_recording() {
        let temp = TempDir::new().unwrap();
        let store = RedbEventStore::open(temp.path().join("events.db")).unwrap();

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
            .record_delivery(seq, "sub1", DeliveryStatus::Delivered)
            .await
            .unwrap();

        let results = store.query_by_subject("test", None, 10).await.unwrap();
        assert_eq!(results[0].deliveries.len(), 1);
        assert_eq!(results[0].deliveries[0].subscriber_id, "sub1");
    }
}
