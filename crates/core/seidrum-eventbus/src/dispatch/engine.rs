use super::subject_index::{SubjectIndex, SubscriptionEntry, SubscriptionMode};
use crate::delivery::ChannelConfig;
use crate::storage::{DeliveryStatus, EventStatus, EventStore, StoredEvent};
use crate::EventBusError;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use tracing::{debug, error, warn};

/// Default channel buffer size for bounded channels.
const DEFAULT_CHANNEL_CAPACITY: usize = 8192;

/// Maximum allowed subject length.
const MAX_SUBJECT_LENGTH: usize = 512;

/// The dispatch engine routes published events to matching subscribers.
pub struct DispatchEngine {
    store: Arc<dyn EventStore>,
    /// Combined lock for index + senders to prevent desync.
    /// Both data structures are always modified atomically together.
    state: Arc<RwLock<DispatchState>>,
}

struct DispatchState {
    index: SubjectIndex,
    senders: HashMap<String, mpsc::Sender<Vec<u8>>>,
}

impl DispatchEngine {
    pub fn new(store: Arc<dyn EventStore>) -> Self {
        Self {
            store,
            state: Arc::new(RwLock::new(DispatchState {
                index: SubjectIndex::new(),
                senders: HashMap::new(),
            })),
        }
    }

    /// Validate a subject string.
    fn validate_subject(subject: &str) -> crate::Result<()> {
        if subject.is_empty() {
            return Err(EventBusError::InvalidSubject(
                "subject must not be empty".to_string(),
            ));
        }
        if subject.len() > MAX_SUBJECT_LENGTH {
            return Err(EventBusError::InvalidSubject(format!(
                "subject exceeds maximum length of {} bytes",
                MAX_SUBJECT_LENGTH
            )));
        }
        if subject.contains('\0') {
            return Err(EventBusError::InvalidSubject(
                "subject must not contain null bytes".to_string(),
            ));
        }
        Ok(())
    }

    pub async fn publish(&self, subject: &str, payload: &[u8]) -> crate::Result<u64> {
        Self::validate_subject(subject)?;

        let event = StoredEvent {
            seq: 0,
            subject: subject.to_string(),
            payload: payload.to_vec(),
            stored_at: Self::current_time_ms(),
            status: EventStatus::Pending,
            deliveries: vec![],
            reply_subject: None,
        };

        // Write-ahead persist
        let seq = self.store.append(&event).await?;

        // Update status to Dispatching
        if let Err(e) = self
            .store
            .update_status(seq, EventStatus::Dispatching)
            .await
        {
            warn!(seq = seq, error = %e, "failed to update event status to Dispatching");
        }

        // Find matching subscriptions (hold read lock for lookup + delivery)
        let state = self.state.read().await;
        let matches = state.index.lookup(subject);

        if matches.is_empty() {
            drop(state);
            // No subscribers — mark as Delivered (vacuously true: all 0 subscribers received it).
            // Note: this is semantically "no subscribers existed", not "delivered to all".
            if let Err(e) = self.store.update_status(seq, EventStatus::Delivered).await {
                warn!(seq = seq, error = %e, "failed to update event status to Delivered (no subscribers)");
            }
            return Ok(seq);
        }

        // Deliver to all matching subscribers (async only in Phase 1)
        let mut any_failed = false;
        for subscription in &matches {
            match &subscription.channel {
                ChannelConfig::InProcess => {
                    if let Some(tx) = state.senders.get(&subscription.id) {
                        match tx.try_send(payload.to_vec()) {
                            Ok(()) => {
                                if let Err(e) = self
                                    .store
                                    .record_delivery(
                                        seq,
                                        &subscription.id,
                                        DeliveryStatus::Delivered,
                                    )
                                    .await
                                {
                                    warn!(
                                        seq = seq,
                                        subscriber = subscription.id,
                                        error = %e,
                                        "failed to record successful delivery"
                                    );
                                }
                            }
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                warn!(
                                    subscriber = subscription.id,
                                    "delivery failed: channel full (backpressure)"
                                );
                                any_failed = true;
                                if let Err(e) = self
                                    .store
                                    .record_delivery(seq, &subscription.id, DeliveryStatus::Failed)
                                    .await
                                {
                                    warn!(
                                        seq = seq,
                                        subscriber = subscription.id,
                                        error = %e,
                                        "failed to record failed delivery"
                                    );
                                }
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                debug!(
                                    subscriber = subscription.id,
                                    "delivery failed: channel closed"
                                );
                                any_failed = true;
                                if let Err(e) = self
                                    .store
                                    .record_delivery(seq, &subscription.id, DeliveryStatus::Failed)
                                    .await
                                {
                                    warn!(
                                        seq = seq,
                                        subscriber = subscription.id,
                                        error = %e,
                                        "failed to record failed delivery"
                                    );
                                }
                            }
                        }
                    } else {
                        debug!(
                            subscriber = subscription.id,
                            "no sender found for subscription"
                        );
                        any_failed = true;
                        if let Err(e) = self
                            .store
                            .record_delivery(seq, &subscription.id, DeliveryStatus::Failed)
                            .await
                        {
                            warn!(
                                seq = seq,
                                subscriber = subscription.id,
                                error = %e,
                                "failed to record failed delivery (no sender)"
                            );
                        }
                    }
                }
                _ => {
                    error!("unsupported channel type in Phase 1");
                    any_failed = true;
                }
            }
        }
        drop(state);

        // Set final status based on delivery outcomes
        let final_status = if any_failed {
            EventStatus::PartiallyDelivered
        } else {
            EventStatus::Delivered
        };
        if let Err(e) = self.store.update_status(seq, final_status).await {
            warn!(seq = seq, error = %e, "failed to update event final status");
        }

        Ok(seq)
    }

    pub async fn subscribe(
        &self,
        subject_pattern: &str,
        priority: u32,
        mode: SubscriptionMode,
        timeout: Duration,
    ) -> crate::Result<(String, mpsc::Receiver<Vec<u8>>)> {
        Self::validate_subject(subject_pattern)?;

        let subscription_id = ulid::Ulid::new().to_string();
        let (tx, rx) = mpsc::channel(DEFAULT_CHANNEL_CAPACITY);

        let entry = SubscriptionEntry {
            id: subscription_id.clone(),
            subject_pattern: subject_pattern.to_string(),
            priority,
            mode,
            channel: ChannelConfig::InProcess,
            timeout,
        };

        // Acquire a single write lock to update both index and senders atomically.
        // This prevents the desync window where a concurrent publish could see
        // the subscription in the index but not find its sender (or vice versa).
        let mut state = self.state.write().await;
        state.index.subscribe(entry);
        state.senders.insert(subscription_id.clone(), tx);

        Ok((subscription_id, rx))
    }

    pub async fn unsubscribe(&self, id: &str) -> crate::Result<()> {
        // Acquire a single write lock to remove from both atomically.
        let mut state = self.state.write().await;
        state.senders.remove(id);
        state.index.unsubscribe(id);

        Ok(())
    }

    pub async fn list_subscriptions(
        &self,
        filter: Option<&str>,
    ) -> crate::Result<Vec<SubscriptionInfo>> {
        let state = self.state.read().await;
        let entries = state.index.list(filter);
        Ok(entries
            .into_iter()
            .map(|e| SubscriptionInfo {
                id: e.id,
                pattern: e.subject_pattern,
                priority: e.priority,
                mode: format!("{:?}", e.mode),
            })
            .collect())
    }

    fn current_time_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

/// Public information about a subscription.
#[derive(Debug, Clone)]
pub struct SubscriptionInfo {
    pub id: String,
    pub pattern: String,
    pub priority: u32,
    pub mode: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::memory_store::InMemoryEventStore;

    #[tokio::test]
    async fn test_publish_with_no_subscribers() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);

        let seq = engine.publish("test.subject", b"payload").await.unwrap();
        assert!(seq > 0);
    }

    #[tokio::test]
    async fn test_subscribe_and_receive() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);

        let (_id, mut rx) = engine
            .subscribe(
                "test.subject",
                10,
                SubscriptionMode::Async,
                Duration::from_secs(5),
            )
            .await
            .unwrap();

        let seq = engine.publish("test.subject", b"hello").await.unwrap();
        assert!(seq > 0);

        let msg = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap();
        assert_eq!(msg, Some(b"hello".to_vec()));
    }

    #[tokio::test]
    async fn test_exact_match_only() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);

        let (_id, mut rx) = engine
            .subscribe(
                "test.a",
                10,
                SubscriptionMode::Async,
                Duration::from_secs(5),
            )
            .await
            .unwrap();

        // Publish to a different subject
        engine.publish("test.b", b"message").await.unwrap();

        // Should not receive
        let msg = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
        assert!(msg.is_err());
    }

    #[tokio::test]
    async fn test_unsubscribe() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);

        let (id, mut rx) = engine
            .subscribe(
                "test.subject",
                10,
                SubscriptionMode::Async,
                Duration::from_secs(5),
            )
            .await
            .unwrap();

        engine.unsubscribe(&id).await.unwrap();
        engine.publish("test.subject", b"message").await.unwrap();

        let msg = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
        // Either timeout or channel closed (None)
        match msg {
            Err(_) => {}   // timeout, fine
            Ok(None) => {} // channel closed, fine
            Ok(Some(_)) => panic!("should not have received message after unsubscribe"),
        }
    }

    #[tokio::test]
    async fn test_multiple_subscribers() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);

        let (_id1, mut rx1) = engine
            .subscribe(
                "test.subject",
                10,
                SubscriptionMode::Async,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        let (_id2, mut rx2) = engine
            .subscribe(
                "test.subject",
                20,
                SubscriptionMode::Async,
                Duration::from_secs(5),
            )
            .await
            .unwrap();

        engine.publish("test.subject", b"message").await.unwrap();

        let msg1 = tokio::time::timeout(std::time::Duration::from_secs(1), rx1.recv())
            .await
            .unwrap();
        let msg2 = tokio::time::timeout(std::time::Duration::from_secs(1), rx2.recv())
            .await
            .unwrap();

        assert_eq!(msg1, Some(b"message".to_vec()));
        assert_eq!(msg2, Some(b"message".to_vec()));
    }

    #[tokio::test]
    async fn test_invalid_subject_empty() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);

        let result = engine.publish("", b"payload").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_invalid_subject_null_byte() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);

        let result = engine.publish("test\0subject", b"payload").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_partially_delivered_status() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store.clone());

        // Subscribe and immediately drop the receiver to simulate a closed channel
        let (_id, rx) = engine
            .subscribe(
                "test.subject",
                10,
                SubscriptionMode::Async,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        drop(rx);

        let seq = engine.publish("test.subject", b"payload").await.unwrap();

        // Event should be PartiallyDelivered since the channel was closed
        let events = store
            .query_by_status(EventStatus::PartiallyDelivered, 10)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, seq);
    }
}
