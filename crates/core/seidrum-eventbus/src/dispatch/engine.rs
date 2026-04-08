use super::subject_index::{SubjectIndex, SubscriptionEntry, SubscriptionMode};
use crate::delivery::ChannelConfig;
use crate::storage::{DeliveryStatus, EventStatus, EventStore, StoredEvent};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use tracing::{debug, error};

/// The dispatch engine routes published events to matching subscribers.
pub struct DispatchEngine {
    store: Arc<dyn EventStore>,
    index: Arc<RwLock<SubjectIndex>>,
    /// Per-subscription senders for in-process delivery.
    senders: Arc<RwLock<HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>>,
}

impl DispatchEngine {
    pub fn new(store: Arc<dyn EventStore>) -> Self {
        Self {
            store,
            index: Arc::new(RwLock::new(SubjectIndex::new())),
            senders: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn publish(&self, subject: &str, payload: &[u8]) -> crate::Result<u64> {
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
        let seq = self
            .store
            .append(&event)
            .await
            .map_err(|e| format!("failed to persist event: {}", e))?;

        // Update status to Dispatching
        self.store
            .update_status(seq, EventStatus::Dispatching)
            .await
            .ok();

        // Find matching subscriptions
        let index = self.index.read().await;
        let matches = index.lookup(subject);
        drop(index);

        if matches.is_empty() {
            // No subscribers, mark as delivered
            self.store
                .update_status(seq, EventStatus::Delivered)
                .await
                .ok();
            return Ok(seq);
        }

        // Deliver to all matching subscribers (async only in Phase 1)
        let senders = self.senders.read().await;
        for subscription in &matches {
            match &subscription.channel {
                ChannelConfig::InProcess => {
                    if let Some(tx) = senders.get(&subscription.id) {
                        match tx.send(payload.to_vec()) {
                            Ok(()) => {
                                self.store
                                    .record_delivery(
                                        seq,
                                        &subscription.id,
                                        DeliveryStatus::Delivered,
                                    )
                                    .await
                                    .ok();
                            }
                            Err(e) => {
                                debug!(
                                    error = %e,
                                    subscriber = subscription.id,
                                    "delivery failed: channel closed"
                                );
                                self.store
                                    .record_delivery(seq, &subscription.id, DeliveryStatus::Failed)
                                    .await
                                    .ok();
                            }
                        }
                    } else {
                        debug!(
                            subscriber = subscription.id,
                            "no sender found for subscription"
                        );
                        self.store
                            .record_delivery(seq, &subscription.id, DeliveryStatus::Failed)
                            .await
                            .ok();
                    }
                }
                _ => {
                    error!("unsupported channel type in Phase 1");
                }
            }
        }
        drop(senders);

        // Mark as delivered
        self.store
            .update_status(seq, EventStatus::Delivered)
            .await
            .ok();

        Ok(seq)
    }

    pub async fn subscribe(
        &self,
        subject_pattern: &str,
        priority: u32,
        mode: SubscriptionMode,
    ) -> crate::Result<(String, mpsc::UnboundedReceiver<Vec<u8>>)> {
        let subscription_id = ulid::Ulid::new().to_string();
        let (tx, rx) = mpsc::unbounded_channel();

        let entry = SubscriptionEntry {
            id: subscription_id.clone(),
            subject_pattern: subject_pattern.to_string(),
            priority,
            mode,
            channel: ChannelConfig::InProcess,
            timeout: std::time::Duration::from_secs(5),
        };

        let mut index = self.index.write().await;
        index.subscribe(entry);
        drop(index);

        let mut senders = self.senders.write().await;
        senders.insert(subscription_id.clone(), tx);

        Ok((subscription_id, rx))
    }

    pub async fn unsubscribe(&self, id: &str) -> crate::Result<()> {
        let mut index = self.index.write().await;
        index.unsubscribe(id);
        drop(index);

        let mut senders = self.senders.write().await;
        senders.remove(id);

        Ok(())
    }

    pub async fn list_subscriptions(
        &self,
        filter: Option<&str>,
    ) -> crate::Result<Vec<SubscriptionInfo>> {
        let index = self.index.read().await;
        let entries = index.list(filter);
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
            .subscribe("test.subject", 10, SubscriptionMode::Async)
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
            .subscribe("test.a", 10, SubscriptionMode::Async)
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
            .subscribe("test.subject", 10, SubscriptionMode::Async)
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
            .subscribe("test.subject", 10, SubscriptionMode::Async)
            .await
            .unwrap();
        let (_id2, mut rx2) = engine
            .subscribe("test.subject", 20, SubscriptionMode::Async)
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
}
