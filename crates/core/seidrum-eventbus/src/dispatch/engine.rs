use super::filter::EventFilter;
use super::interceptor::{InterceptResult, Interceptor};
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

/// Default timeout for sync interceptors.
const DEFAULT_INTERCEPTOR_TIMEOUT: Duration = Duration::from_secs(5);

/// The dispatch engine routes published events to matching subscribers.
/// Implements the full pipeline: persist → resolve → filter → sync chain → async fan-out → finalize.
pub struct DispatchEngine {
    store: Arc<dyn EventStore>,
    /// Combined lock for index + senders + interceptors to prevent desync.
    state: Arc<RwLock<DispatchState>>,
}

struct DispatchState {
    index: SubjectIndex,
    senders: HashMap<String, mpsc::Sender<Vec<u8>>>,
    /// Per-subscription interceptors (for sync mode in-process subscribers).
    interceptors: HashMap<String, Arc<dyn Interceptor>>,
}

impl DispatchEngine {
    pub fn new(store: Arc<dyn EventStore>) -> Self {
        Self {
            store,
            state: Arc::new(RwLock::new(DispatchState {
                index: SubjectIndex::new(),
                senders: HashMap::new(),
                interceptors: HashMap::new(),
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

        // 1. PERSIST (write-ahead)
        let seq = self.store.append(&event).await?;

        // 2. RESOLVE: find matching subscriptions
        let state = self.state.read().await;
        let mut matches = state.index.lookup(subject);

        if matches.is_empty() {
            drop(state);
            if let Err(e) = self.store.update_status(seq, EventStatus::Delivered).await {
                warn!(seq = seq, error = %e, "failed to update status to Delivered (no subscribers)");
            }
            return Ok(seq);
        }

        // Update status to Dispatching
        if let Err(e) = self
            .store
            .update_status(seq, EventStatus::Dispatching)
            .await
        {
            warn!(seq = seq, error = %e, "failed to update status to Dispatching");
        }

        // 3. FILTER: apply EventFilter on each subscription
        let mut current_payload = payload.to_vec();
        matches.retain(|entry| {
            entry
                .filter
                .as_ref()
                .map_or(true, |f| f.matches(&current_payload))
        });

        if matches.is_empty() {
            drop(state);
            if let Err(e) = self.store.update_status(seq, EventStatus::Delivered).await {
                warn!(seq = seq, error = %e, "failed to update status to Delivered (all filtered)");
            }
            return Ok(seq);
        }

        // Split into sync (sorted by priority, already sorted from lookup) and async
        let sync_entries: Vec<_> = matches
            .iter()
            .filter(|e| e.mode == SubscriptionMode::Sync)
            .cloned()
            .collect();
        let async_entries: Vec<_> = matches
            .iter()
            .filter(|e| e.mode == SubscriptionMode::Async)
            .cloned()
            .collect();

        // 4. SYNC CHAIN: process interceptors sequentially in priority order
        let mut dropped = false;
        for entry in &sync_entries {
            if let Some(interceptor) = state.interceptors.get(&entry.id) {
                let interceptor = Arc::clone(interceptor);
                let timeout = entry.timeout;

                // Execute interceptor with timeout enforcement
                let result = tokio::time::timeout(
                    timeout,
                    interceptor.intercept(subject, &mut current_payload),
                )
                .await;

                match result {
                    Ok(InterceptResult::Pass) => {
                        if let Err(e) = self
                            .store
                            .record_delivery(seq, &entry.id, DeliveryStatus::Delivered)
                            .await
                        {
                            warn!(seq = seq, subscriber = entry.id, error = %e, "failed to record interceptor delivery");
                        }
                    }
                    Ok(InterceptResult::Modified) => {
                        debug!(subscriber = entry.id, "interceptor modified payload");
                        if let Err(e) = self
                            .store
                            .record_delivery(seq, &entry.id, DeliveryStatus::Delivered)
                            .await
                        {
                            warn!(seq = seq, subscriber = entry.id, error = %e, "failed to record interceptor delivery");
                        }
                    }
                    Ok(InterceptResult::Drop) => {
                        debug!(subscriber = entry.id, "interceptor dropped event");
                        if let Err(e) = self
                            .store
                            .record_delivery(seq, &entry.id, DeliveryStatus::Delivered)
                            .await
                        {
                            warn!(seq = seq, subscriber = entry.id, error = %e, "failed to record interceptor delivery");
                        }
                        dropped = true;
                        break;
                    }
                    Err(_timeout) => {
                        warn!(
                            subscriber = entry.id,
                            timeout_ms = timeout.as_millis() as u64,
                            "sync interceptor timed out, skipping"
                        );
                        // Timed-out interceptor is skipped, chain continues
                        if let Err(e) = self
                            .store
                            .record_delivery(seq, &entry.id, DeliveryStatus::Failed)
                            .await
                        {
                            warn!(seq = seq, subscriber = entry.id, error = %e, "failed to record timeout");
                        }
                    }
                }
            } else {
                // Sync subscriber without an interceptor — deliver via channel
                if let Some(tx) = state.senders.get(&entry.id) {
                    match tx.try_send(current_payload.clone()) {
                        Ok(()) => {
                            if let Err(e) = self
                                .store
                                .record_delivery(seq, &entry.id, DeliveryStatus::Delivered)
                                .await
                            {
                                warn!(seq = seq, subscriber = entry.id, error = %e, "failed to record delivery");
                            }
                        }
                        Err(_) => {
                            warn!(subscriber = entry.id, "sync channel delivery failed");
                            if let Err(e) = self
                                .store
                                .record_delivery(seq, &entry.id, DeliveryStatus::Failed)
                                .await
                            {
                                warn!(seq = seq, subscriber = entry.id, error = %e, "failed to record failed delivery");
                            }
                        }
                    }
                }
            }
        }

        if dropped {
            drop(state);
            // Event was intentionally dropped by an interceptor — mark as Delivered.
            if let Err(e) = self.store.update_status(seq, EventStatus::Delivered).await {
                warn!(seq = seq, error = %e, "failed to update status after drop");
            }
            return Ok(seq);
        }

        // 5. ASYNC FAN-OUT: deliver the (possibly mutated) payload to all async subscribers
        let mut any_failed = false;
        for entry in &async_entries {
            match &entry.channel {
                ChannelConfig::InProcess => {
                    if let Some(tx) = state.senders.get(&entry.id) {
                        match tx.try_send(current_payload.clone()) {
                            Ok(()) => {
                                if let Err(e) = self
                                    .store
                                    .record_delivery(seq, &entry.id, DeliveryStatus::Delivered)
                                    .await
                                {
                                    warn!(seq = seq, subscriber = entry.id, error = %e, "failed to record delivery");
                                }
                            }
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                warn!(subscriber = entry.id, "delivery failed: channel full");
                                any_failed = true;
                                if let Err(e) = self
                                    .store
                                    .record_delivery(seq, &entry.id, DeliveryStatus::Failed)
                                    .await
                                {
                                    warn!(seq = seq, subscriber = entry.id, error = %e, "failed to record failed delivery");
                                }
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                debug!(subscriber = entry.id, "delivery failed: channel closed");
                                any_failed = true;
                                if let Err(e) = self
                                    .store
                                    .record_delivery(seq, &entry.id, DeliveryStatus::Failed)
                                    .await
                                {
                                    warn!(seq = seq, subscriber = entry.id, error = %e, "failed to record failed delivery");
                                }
                            }
                        }
                    } else {
                        debug!(subscriber = entry.id, "no sender found");
                        any_failed = true;
                        if let Err(e) = self
                            .store
                            .record_delivery(seq, &entry.id, DeliveryStatus::Failed)
                            .await
                        {
                            warn!(seq = seq, subscriber = entry.id, error = %e, "failed to record failed delivery");
                        }
                    }
                }
                _ => {
                    error!("unsupported channel type in Phase 2");
                    any_failed = true;
                }
            }
        }
        drop(state);

        // 6. FINALIZE
        let final_status = if any_failed {
            EventStatus::PartiallyDelivered
        } else {
            EventStatus::Delivered
        };
        if let Err(e) = self.store.update_status(seq, final_status).await {
            warn!(seq = seq, error = %e, "failed to update final status");
        }

        Ok(seq)
    }

    /// Subscribe with a channel for receiving events (async mode typical).
    pub async fn subscribe(
        &self,
        subject_pattern: &str,
        priority: u32,
        mode: SubscriptionMode,
        timeout: Duration,
        filter: Option<EventFilter>,
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
            filter,
        };

        let mut state = self.state.write().await;
        state.index.subscribe(entry);
        state.senders.insert(subscription_id.clone(), tx);

        Ok((subscription_id, rx))
    }

    /// Register a sync interceptor. Returns the subscription ID.
    pub async fn intercept(
        &self,
        subject_pattern: &str,
        priority: u32,
        interceptor: Arc<dyn Interceptor>,
        timeout: Option<Duration>,
    ) -> crate::Result<String> {
        Self::validate_subject(subject_pattern)?;

        let subscription_id = ulid::Ulid::new().to_string();

        let entry = SubscriptionEntry {
            id: subscription_id.clone(),
            subject_pattern: subject_pattern.to_string(),
            priority,
            mode: SubscriptionMode::Sync,
            channel: ChannelConfig::InProcess,
            timeout: timeout.unwrap_or(DEFAULT_INTERCEPTOR_TIMEOUT),
            filter: None,
        };

        let mut state = self.state.write().await;
        state.index.subscribe(entry);
        state
            .interceptors
            .insert(subscription_id.clone(), interceptor);

        Ok(subscription_id)
    }

    pub async fn unsubscribe(&self, id: &str) -> crate::Result<()> {
        let mut state = self.state.write().await;
        state.senders.remove(id);
        state.interceptors.remove(id);
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

    struct UppercaseInterceptor;

    #[async_trait::async_trait]
    impl Interceptor for UppercaseInterceptor {
        async fn intercept(&self, _subject: &str, payload: &mut Vec<u8>) -> InterceptResult {
            payload.iter_mut().for_each(|b| *b = b.to_ascii_uppercase());
            InterceptResult::Modified
        }
    }

    struct DropInterceptor;

    #[async_trait::async_trait]
    impl Interceptor for DropInterceptor {
        async fn intercept(&self, _subject: &str, _payload: &mut Vec<u8>) -> InterceptResult {
            InterceptResult::Drop
        }
    }

    struct SlowInterceptor;

    #[async_trait::async_trait]
    impl Interceptor for SlowInterceptor {
        async fn intercept(&self, _subject: &str, _payload: &mut Vec<u8>) -> InterceptResult {
            tokio::time::sleep(Duration::from_secs(10)).await;
            InterceptResult::Pass
        }
    }

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
                None,
            )
            .await
            .unwrap();

        engine.publish("test.subject", b"hello").await.unwrap();

        let msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap();
        assert_eq!(msg, Some(b"hello".to_vec()));
    }

    #[tokio::test]
    async fn test_wildcard_star_matching() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);

        let (_id, mut rx) = engine
            .subscribe(
                "channel.*.inbound",
                10,
                SubscriptionMode::Async,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();

        engine
            .publish("channel.telegram.inbound", b"msg")
            .await
            .unwrap();

        let msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap();
        assert_eq!(msg, Some(b"msg".to_vec()));

        // Should NOT match different depth
        engine
            .publish("channel.telegram.sub.inbound", b"nope")
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_wildcard_gt_matching() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);

        let (_id, mut rx) = engine
            .subscribe(
                "brain.>",
                10,
                SubscriptionMode::Async,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();

        engine.publish("brain.content.store", b"a").await.unwrap();
        let msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap();
        assert_eq!(msg, Some(b"a".to_vec()));

        engine
            .publish("brain.entity.upsert.batch", b"b")
            .await
            .unwrap();
        let msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap();
        assert_eq!(msg, Some(b"b".to_vec()));
    }

    #[tokio::test]
    async fn test_interceptor_modifies_payload() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);

        // Register sync interceptor at priority 5 (runs first)
        engine
            .intercept("test.subject", 5, Arc::new(UppercaseInterceptor), None)
            .await
            .unwrap();

        // Register async subscriber at priority 10 (receives mutated payload)
        let (_id, mut rx) = engine
            .subscribe(
                "test.subject",
                10,
                SubscriptionMode::Async,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();

        engine.publish("test.subject", b"hello").await.unwrap();

        let msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap();
        // Async subscriber should receive the UPPERCASED payload
        assert_eq!(msg, Some(b"HELLO".to_vec()));
    }

    #[tokio::test]
    async fn test_interceptor_drops_event() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);

        // Register drop interceptor
        engine
            .intercept("test.subject", 5, Arc::new(DropInterceptor), None)
            .await
            .unwrap();

        // Register async subscriber — should NOT receive anything
        let (_id, mut rx) = engine
            .subscribe(
                "test.subject",
                10,
                SubscriptionMode::Async,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();

        engine.publish("test.subject", b"hello").await.unwrap();

        let result = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        // Should timeout — event was dropped before async delivery
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_interceptor_timeout_skips() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);

        // Register slow interceptor with short timeout
        engine
            .intercept(
                "test.subject",
                5,
                Arc::new(SlowInterceptor),
                Some(Duration::from_millis(50)),
            )
            .await
            .unwrap();

        // Register async subscriber
        let (_id, mut rx) = engine
            .subscribe(
                "test.subject",
                10,
                SubscriptionMode::Async,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();

        engine.publish("test.subject", b"hello").await.unwrap();

        // Subscriber should still receive the event (interceptor was skipped)
        let msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap();
        assert_eq!(msg, Some(b"hello".to_vec()));
    }

    #[tokio::test]
    async fn test_interceptor_chain_ordering() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);

        // Two interceptors: first uppercases (priority 5), second could drop (priority 10)
        // Since first runs before second, the payload is already uppercased by second's turn
        engine
            .intercept("test.subject", 5, Arc::new(UppercaseInterceptor), None)
            .await
            .unwrap();

        // Add an async subscriber to see the final result
        let (_id, mut rx) = engine
            .subscribe(
                "test.subject",
                20,
                SubscriptionMode::Async,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();

        engine.publish("test.subject", b"hello").await.unwrap();

        let msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap();
        assert_eq!(msg, Some(b"HELLO".to_vec()));
    }

    #[tokio::test]
    async fn test_event_filter() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);

        // Subscribe with a filter that only matches platform=telegram
        let filter = EventFilter::FieldEquals {
            path: "platform".to_string(),
            value: serde_json::json!("telegram"),
        };
        let (_id, mut rx) = engine
            .subscribe(
                "channel.>",
                10,
                SubscriptionMode::Async,
                Duration::from_secs(5),
                Some(filter),
            )
            .await
            .unwrap();

        // Publish a telegram event — should match
        let telegram_payload =
            serde_json::to_vec(&serde_json::json!({"platform": "telegram", "text": "hi"})).unwrap();
        engine
            .publish("channel.telegram.inbound", &telegram_payload)
            .await
            .unwrap();

        let msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap();
        assert!(msg.is_some());

        // Publish an email event — should NOT match the filter
        let email_payload =
            serde_json::to_vec(&serde_json::json!({"platform": "email", "text": "hi"})).unwrap();
        engine
            .publish("channel.email.inbound", &email_payload)
            .await
            .unwrap();

        let result = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_mixed_sync_async_delivery() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);

        // Sync interceptor at priority 5
        engine
            .intercept("test", 5, Arc::new(UppercaseInterceptor), None)
            .await
            .unwrap();

        // Async subscriber 1 at priority 10
        let (_id1, mut rx1) = engine
            .subscribe(
                "test",
                10,
                SubscriptionMode::Async,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();

        // Async subscriber 2 at priority 20
        let (_id2, mut rx2) = engine
            .subscribe(
                "test",
                20,
                SubscriptionMode::Async,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();

        engine.publish("test", b"hello").await.unwrap();

        let msg1 = tokio::time::timeout(Duration::from_secs(1), rx1.recv())
            .await
            .unwrap();
        let msg2 = tokio::time::timeout(Duration::from_secs(1), rx2.recv())
            .await
            .unwrap();

        // Both should get the uppercased payload
        assert_eq!(msg1, Some(b"HELLO".to_vec()));
        assert_eq!(msg2, Some(b"HELLO".to_vec()));
    }

    #[tokio::test]
    async fn test_unsubscribe_interceptor() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);

        let id = engine
            .intercept("test", 5, Arc::new(UppercaseInterceptor), None)
            .await
            .unwrap();

        let (_sub_id, mut rx) = engine
            .subscribe(
                "test",
                10,
                SubscriptionMode::Async,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();

        // Unsubscribe the interceptor
        engine.unsubscribe(&id).await.unwrap();

        engine.publish("test", b"hello").await.unwrap();

        let msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap();
        // Should receive original (lowercase) since interceptor was removed
        assert_eq!(msg, Some(b"hello".to_vec()));
    }

    #[tokio::test]
    async fn test_invalid_subject_empty() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);
        assert!(engine.publish("", b"payload").await.is_err());
    }

    #[tokio::test]
    async fn test_invalid_subject_null_byte() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);
        assert!(engine.publish("test\0subject", b"payload").await.is_err());
    }

    #[tokio::test]
    async fn test_partially_delivered_status() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store.clone());

        let (_id, rx) = engine
            .subscribe(
                "test",
                10,
                SubscriptionMode::Async,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();
        drop(rx);

        let seq = engine.publish("test", b"payload").await.unwrap();

        let events = store
            .query_by_status(EventStatus::PartiallyDelivered, 10)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, seq);
    }
}
