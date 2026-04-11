use super::{
    DeliveryRecord, DeliveryStatus, EventStatus, EventStore, PersistedSubscription,
    RetryableDelivery, StorageError, StorageResult, StoredEvent,
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
/// Index of events that have at least one delivery in the `Failed` state.
/// Key: `seq`. Used by `query_retryable` to skip events with no failed
/// deliveries instead of full-scanning the events table.
const FAILED_DELIVERIES_IDX_TABLE: redb::TableDefinition<u64, ()> =
    redb::TableDefinition::new("failed_deliveries_idx");
/// Persisted webhook subscriptions, keyed by persisted_id (a ULID string).
/// Used by the HTTP transport so subscriptions survive process restarts.
const SUBSCRIPTIONS_TABLE: redb::TableDefinition<&str, &[u8]> =
    redb::TableDefinition::new("subscriptions");

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
                let _t4 = write_txn
                    .open_table(FAILED_DELIVERIES_IDX_TABLE)
                    .map_err(|e| {
                        StorageError::DatabaseError(format!(
                            "failed to open failed_deliveries_idx table: {}",
                            e
                        ))
                    })?;
                let _t5 = write_txn.open_table(SUBSCRIPTIONS_TABLE).map_err(|e| {
                    StorageError::DatabaseError(format!(
                        "failed to open subscriptions table: {}",
                        e
                    ))
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

    async fn get(&self, seq: u64) -> StorageResult<Option<StoredEvent>> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            let read_txn = db
                .begin_read()
                .map_err(|e| StorageError::DatabaseError(format!("failed to begin read: {}", e)))?;
            let events_table = read_txn.open_table(EVENTS_TABLE).map_err(|e| {
                StorageError::DatabaseError(format!("failed to open events table: {}", e))
            })?;
            match events_table
                .get(seq)
                .map_err(|e| StorageError::DatabaseError(format!("failed to get event: {}", e)))?
            {
                Some(data) => {
                    let bytes = data.value().to_vec();
                    let event: StoredEvent = serde_json::from_slice(&bytes).map_err(|e| {
                        StorageError::OperationFailed(format!("failed to deserialize: {}", e))
                    })?;
                    Ok(Some(event))
                }
                None => Ok(None),
            }
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

            // NOTE: We deliberately do NOT touch failed_deliveries_idx here.
            // The invariant "seq in index iff any delivery is Failed" is
            // maintained exclusively by record_delivery, which has full
            // visibility into the deliveries Vec.
            //
            // An earlier version "defensively" purged the index entry on
            // transitions to terminal status, but update_status does not
            // mutate the deliveries Vec — so callers like the dispatch
            // engine's interceptor-Drop path (which records Failed
            // deliveries before flipping status to Delivered) would silently
            // strand failed deliveries that the retry task could no longer
            // see.

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
        error: Option<String>,
        next_retry: Option<u64>,
    ) -> StorageResult<()> {
        let db = Arc::clone(&self.db);
        let subscriber_id = subscriber_id.to_string();

        tokio::task::spawn_blocking(move || {
            let write_txn = db.begin_write().map_err(|e| {
                StorageError::DatabaseError(format!("failed to begin write: {}", e))
            })?;

            // Read the event, mutate the delivery record, and capture the
            // post-update state of the failed-deliveries index.
            let (serialized, has_failed_deliveries) = {
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
                    // Count attempts that represent a real delivery attempt
                    // (Failed and DeadLettered) but not Delivered (off-by-one
                    // observability).
                    if matches!(
                        status,
                        DeliveryStatus::Failed | DeliveryStatus::DeadLettered
                    ) {
                        delivery.attempts += 1;
                    }
                    delivery.last_attempt = Some(RedbEventStore::current_time_ms());
                    // Only overwrite the error on Failed/DeadLettered
                    // transitions. Successful retries preserve the prior
                    // failure context for diagnostics, regardless of what
                    // the caller passed.
                    if matches!(
                        status,
                        DeliveryStatus::Failed | DeliveryStatus::DeadLettered
                    ) {
                        delivery.error = error;
                    }
                    delivery.next_retry = next_retry;
                } else {
                    let now = RedbEventStore::current_time_ms();
                    event.deliveries.push(DeliveryRecord {
                        subscriber_id,
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

                let has_failed = event
                    .deliveries
                    .iter()
                    .any(|d| d.status == DeliveryStatus::Failed);

                let serialized = serde_json::to_vec(&event).map_err(|e| {
                    StorageError::OperationFailed(format!("failed to serialize event: {}", e))
                })?;
                (serialized, has_failed)
            };

            // Write back the event
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

            // Maintain the failed-deliveries index: insert if any delivery
            // is currently Failed, otherwise remove the entry.
            {
                let mut failed_idx =
                    write_txn
                        .open_table(FAILED_DELIVERIES_IDX_TABLE)
                        .map_err(|e| {
                            StorageError::DatabaseError(format!(
                                "failed to open failed_deliveries_idx: {}",
                                e
                            ))
                        })?;
                if has_failed_deliveries {
                    failed_idx.insert(seq, ()).map_err(|e| {
                        StorageError::DatabaseError(format!(
                            "failed to insert failed_deliveries_idx: {}",
                            e
                        ))
                    })?;
                } else {
                    let _ = failed_idx.remove(seq);
                }
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
                    // Verify actual status matches index (guards against stale entries).
                    if event.status == status {
                        results.push(event);
                    }
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

        tokio::task::spawn_blocking(move || {
            let read_txn = db
                .begin_read()
                .map_err(|e| StorageError::DatabaseError(format!("failed to begin read: {}", e)))?;
            let events_table = read_txn.open_table(EVENTS_TABLE).map_err(|e| {
                StorageError::DatabaseError(format!("failed to open events table: {}", e))
            })?;
            let failed_idx = read_txn
                .open_table(FAILED_DELIVERIES_IDX_TABLE)
                .map_err(|e| {
                    StorageError::DatabaseError(format!(
                        "failed to open failed_deliveries_idx: {}",
                        e
                    ))
                })?;

            let mut results: Vec<RetryableDelivery> = Vec::new();
            // Iterate only events known to have at least one Failed delivery.
            let iter = failed_idx
                .iter()
                .map_err(|e| StorageError::DatabaseError(format!("failed to iterate: {}", e)))?;

            let now = RedbEventStore::current_time_ms();
            for item in iter {
                let (key, _) = item.map_err(|e| {
                    StorageError::DatabaseError(format!("failed to iterate: {}", e))
                })?;
                let seq: u64 = key.value();

                let data = match events_table.get(seq).map_err(|e| {
                    StorageError::DatabaseError(format!("failed to get event: {}", e))
                })? {
                    Some(d) => d,
                    None => {
                        tracing::warn!(
                            seq = seq,
                            "stale failed_deliveries_idx entry: event not found"
                        );
                        continue;
                    }
                };
                let bytes = data.value().to_vec();
                let event: StoredEvent = serde_json::from_slice(&bytes).map_err(|e| {
                    StorageError::OperationFailed(format!("failed to deserialize: {}", e))
                })?;

                for delivery in &event.deliveries {
                    if delivery.is_retryable(max_attempts, now) {
                        results.push(RetryableDelivery {
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

            // Sort by next_retry ascending so the earliest-due deliveries
            // come first. None (no retry scheduled) sorts to the front.
            //
            // PERF: For very large failed-deliveries backlogs (10k+) this
            // is O(n log n) where n is the backlog size, then truncated
            // to `limit`. A bounded min-heap would be O(n log limit) but
            // is not worth the code complexity at current scale.
            results.sort_by_key(|r| r.next_retry.unwrap_or(0));
            results.truncate(limit);
            Ok(results)
        })
        .await
        .map_err(|e| StorageError::OperationFailed(format!("spawn_blocking error: {}", e)))?
    }

    async fn count_retryable(&self, max_attempts: u32) -> StorageResult<u64> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            let read_txn = db
                .begin_read()
                .map_err(|e| StorageError::DatabaseError(format!("failed to begin read: {}", e)))?;
            let events_table = read_txn.open_table(EVENTS_TABLE).map_err(|e| {
                StorageError::DatabaseError(format!("failed to open events table: {}", e))
            })?;
            let failed_idx = read_txn
                .open_table(FAILED_DELIVERIES_IDX_TABLE)
                .map_err(|e| {
                    StorageError::DatabaseError(format!(
                        "failed to open failed_deliveries_idx: {}",
                        e
                    ))
                })?;

            let now = RedbEventStore::current_time_ms();
            let mut count = 0u64;
            let iter = failed_idx
                .iter()
                .map_err(|e| StorageError::DatabaseError(format!("failed to iterate: {}", e)))?;
            for item in iter {
                let (key, _) = item.map_err(|e| {
                    StorageError::DatabaseError(format!("failed to iterate: {}", e))
                })?;
                let seq: u64 = key.value();
                let data = match events_table.get(seq).map_err(|e| {
                    StorageError::DatabaseError(format!("failed to get event: {}", e))
                })? {
                    Some(d) => d,
                    None => {
                        tracing::warn!(
                            seq = seq,
                            "stale failed_deliveries_idx entry: event not found"
                        );
                        continue;
                    }
                };
                let bytes = data.value().to_vec();
                let event: StoredEvent = serde_json::from_slice(&bytes).map_err(|e| {
                    StorageError::OperationFailed(format!("failed to deserialize: {}", e))
                })?;
                for delivery in &event.deliveries {
                    if delivery.is_retryable(max_attempts, now) {
                        count += 1;
                    }
                }
            }
            Ok(count)
        })
        .await
        .map_err(|e| StorageError::OperationFailed(format!("spawn_blocking error: {}", e)))?
    }

    async fn compact(&self, older_than: Duration) -> StorageResult<u64> {
        let db = Arc::clone(&self.db);
        let threshold =
            RedbEventStore::current_time_ms().saturating_sub(older_than.as_millis() as u64);

        tokio::task::spawn_blocking(move || {
            // Single write transaction: identify candidates and delete atomically.
            // This prevents TOCTOU races where an event's status could change
            // between identification and deletion.
            let write_txn = db.begin_write().map_err(|e| {
                StorageError::DatabaseError(format!("failed to begin write: {}", e))
            })?;

            let to_delete: Vec<(u64, String, u8)> = {
                let events_table = write_txn.open_table(EVENTS_TABLE).map_err(|e| {
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

                    let terminal = matches!(
                        event.status,
                        EventStatus::Delivered | EventStatus::DeadLettered
                    );
                    if terminal && event.stored_at < threshold {
                        items.push((seq, event.subject, event.status.as_u8()));
                    }
                }
                items
            };

            if to_delete.is_empty() {
                // Abort the write transaction — nothing to do.
                let _ = write_txn.abort();
                return Ok(0);
            }

            let removed = to_delete.len() as u64;

            // Open tables once outside the loop to avoid repeated handle setup.
            let mut events_table = write_txn.open_table(EVENTS_TABLE).map_err(|e| {
                StorageError::DatabaseError(format!("failed to open events table: {}", e))
            })?;
            let mut subject_idx = write_txn.open_table(SUBJECT_IDX_TABLE).map_err(|e| {
                StorageError::DatabaseError(format!("failed to open subject_idx: {}", e))
            })?;
            let mut status_idx = write_txn.open_table(STATUS_IDX_TABLE).map_err(|e| {
                StorageError::DatabaseError(format!("failed to open status_idx: {}", e))
            })?;
            let mut failed_idx = write_txn.open_table(FAILED_DELIVERIES_IDX_TABLE).map_err(|e| {
                StorageError::DatabaseError(format!("failed to open failed_deliveries_idx: {}", e))
            })?;

            for (seq, subject, status_u8) in &to_delete {
                if let Err(e) = events_table.remove(*seq) {
                    tracing::warn!(seq = seq, error = %e, "compaction failed to remove event");
                }
                if let Err(e) = subject_idx.remove((subject.as_str(), *seq)) {
                    tracing::warn!(seq = seq, subject = %subject, error = %e, "compaction failed to remove subject index entry");
                }
                if let Err(e) = status_idx.remove((*status_u8, *seq)) {
                    tracing::warn!(seq = seq, status = status_u8, error = %e, "compaction failed to remove status index entry");
                }
                // Best-effort: also drop the failed-deliveries index entry.
                let _ = failed_idx.remove(*seq);
            }
            // Drop the table handles before commit (required by redb borrow rules).
            drop(events_table);
            drop(subject_idx);
            drop(status_idx);
            drop(failed_idx);

            write_txn
                .commit()
                .map_err(|e| StorageError::DatabaseError(format!("failed to commit: {}", e)))?;

            Ok(removed)
        })
        .await
        .map_err(|e| StorageError::OperationFailed(format!("spawn_blocking error: {}", e)))?
    }

    async fn save_subscription(&self, sub: &PersistedSubscription) -> StorageResult<()> {
        let db = Arc::clone(&self.db);
        let sub = sub.clone();
        tokio::task::spawn_blocking(move || {
            let serialized = serde_json::to_vec(&sub).map_err(|e| {
                StorageError::OperationFailed(format!("serialize subscription: {}", e))
            })?;
            let write_txn = db.begin_write().map_err(|e| {
                StorageError::DatabaseError(format!("begin write: {}", e))
            })?;
            {
                let mut t = write_txn.open_table(SUBSCRIPTIONS_TABLE).map_err(|e| {
                    StorageError::DatabaseError(format!(
                        "open subscriptions table: {}",
                        e
                    ))
                })?;
                t.insert(sub.persisted_id.as_str(), serialized.as_slice())
                    .map_err(|e| {
                        StorageError::DatabaseError(format!("insert subscription: {}", e))
                    })?;
            }
            write_txn
                .commit()
                .map_err(|e| StorageError::DatabaseError(format!("commit: {}", e)))
        })
        .await
        .map_err(|e| StorageError::OperationFailed(format!("spawn_blocking: {}", e)))?
    }

    async fn list_subscriptions(&self) -> StorageResult<Vec<PersistedSubscription>> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            let read_txn = db
                .begin_read()
                .map_err(|e| StorageError::DatabaseError(format!("begin read: {}", e)))?;
            let table = read_txn.open_table(SUBSCRIPTIONS_TABLE).map_err(|e| {
                StorageError::DatabaseError(format!("open subscriptions: {}", e))
            })?;
            let mut results = Vec::new();
            let iter = table
                .iter()
                .map_err(|e| StorageError::DatabaseError(format!("iterate: {}", e)))?;
            for item in iter {
                let (_, value) = item.map_err(|e| {
                    StorageError::DatabaseError(format!("iterate row: {}", e))
                })?;
                let bytes = value.value().to_vec();
                let sub: PersistedSubscription =
                    serde_json::from_slice(&bytes).map_err(|e| {
                        StorageError::OperationFailed(format!(
                            "deserialize subscription: {}",
                            e
                        ))
                    })?;
                results.push(sub);
            }
            Ok(results)
        })
        .await
        .map_err(|e| StorageError::OperationFailed(format!("spawn_blocking: {}", e)))?
    }

    async fn delete_subscription(&self, persisted_id: &str) -> StorageResult<()> {
        let db = Arc::clone(&self.db);
        let persisted_id = persisted_id.to_string();
        tokio::task::spawn_blocking(move || {
            let write_txn = db
                .begin_write()
                .map_err(|e| StorageError::DatabaseError(format!("begin write: {}", e)))?;
            {
                let mut t = write_txn.open_table(SUBSCRIPTIONS_TABLE).map_err(|e| {
                    StorageError::DatabaseError(format!(
                        "open subscriptions table: {}",
                        e
                    ))
                })?;
                let _ = t.remove(persisted_id.as_str());
            }
            write_txn
                .commit()
                .map_err(|e| StorageError::DatabaseError(format!("commit: {}", e)))
        })
        .await
        .map_err(|e| StorageError::OperationFailed(format!("spawn_blocking: {}", e)))?
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
            .record_delivery(seq, "sub1", DeliveryStatus::Delivered, None, None)
            .await
            .unwrap();

        let results = store.query_by_subject("test", None, 10).await.unwrap();
        assert_eq!(results[0].deliveries.len(), 1);
        assert_eq!(results[0].deliveries[0].subscriber_id, "sub1");
    }

    /// Required by QUALITY.md L73: concurrent appends do not corrupt data.
    /// 50 concurrent appends from 10 tasks must produce 50 distinct
    /// monotonic sequence numbers and 50 readable events.
    #[tokio::test]
    async fn test_redb_concurrent_appends() {
        let temp = TempDir::new().unwrap();
        let store = Arc::new(RedbEventStore::open(temp.path().join("events.db")).unwrap());

        let mut handles = vec![];
        for task_id in 0..10 {
            let store = Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                let mut seqs = vec![];
                for i in 0..5 {
                    let event = StoredEvent {
                        seq: 0,
                        subject: format!("test.task.{}", task_id),
                        payload: format!("event-{}-{}", task_id, i).into_bytes(),
                        stored_at: 0,
                        status: EventStatus::Pending,
                        deliveries: vec![],
                        reply_subject: None,
                    };
                    seqs.push(store.append(&event).await.unwrap());
                }
                seqs
            }));
        }

        let mut all_seqs: Vec<u64> = Vec::new();
        for handle in handles {
            let seqs = handle.await.unwrap();
            all_seqs.extend(seqs);
        }

        // 50 distinct, monotonic sequence numbers.
        assert_eq!(all_seqs.len(), 50);
        let mut sorted = all_seqs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 50, "all sequence numbers must be unique");

        // Every event is readable via get(seq).
        for seq in &all_seqs {
            let event = store.get(*seq).await.unwrap();
            assert!(event.is_some(), "event seq={} should be readable", seq);
        }
    }

    /// Regression test: a Failed delivery must remain queryable via
    /// `query_retryable` even if the parent event's status was bumped to
    /// `Delivered` by an unrelated path (e.g., the dispatch engine's
    /// interceptor-Drop branch). The original Phase 5 implementation had
    /// `update_status` defensively purge `failed_deliveries_idx`, which
    /// silently dropped failed deliveries.
    #[tokio::test]
    async fn test_redb_update_status_preserves_failed_index() {
        let temp = TempDir::new().unwrap();
        let store = RedbEventStore::open(temp.path().join("events.db")).unwrap();

        // Append an event and record a Failed delivery against it.
        let event = StoredEvent {
            seq: 0,
            subject: "test".to_string(),
            payload: b"data".to_vec(),
            stored_at: 1000,
            status: EventStatus::Dispatching,
            deliveries: vec![],
            reply_subject: None,
        };
        let seq = store.append(&event).await.unwrap();
        store
            .record_delivery(
                seq,
                "sub1",
                DeliveryStatus::Failed,
                Some("boom".to_string()),
                Some(0), // due immediately
            )
            .await
            .unwrap();

        // Sanity: the failed delivery is visible.
        let pending = store.query_retryable(10, 100).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].subscriber_id, "sub1");

        // Bump the event status to Delivered (simulating the dispatch
        // engine's interceptor-Drop branch).
        store
            .update_status(seq, EventStatus::Delivered)
            .await
            .unwrap();

        // The failed delivery MUST still be retryable.
        let still_pending = store.query_retryable(10, 100).await.unwrap();
        assert_eq!(
            still_pending.len(),
            1,
            "Failed delivery should survive update_status -> Delivered"
        );
        assert_eq!(still_pending[0].subscriber_id, "sub1");
    }
}
