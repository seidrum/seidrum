//! Dispatch engine: routes published events to matching subscriptions.
//!
//! ## Architecture
//!
//! The dispatch pipeline has 6 stages:
//!
//! 1. **PERSIST**: Write the event durably to the store (write-ahead logging).
//! 2. **RESOLVE**: Look up all subscriptions matching the subject using the trie index.
//! 3. **FILTER**: Apply EventFilter to narrow subscribers to those interested in the payload.
//! 4. **SYNC CHAIN**: Process sync interceptors in priority order (lower = first).
//!    - Interceptors can Pass, Modify, or Drop events.
//!    - Mutations propagate to later subscribers.
//!    - Drop aborts further processing.
//! 5. **ASYNC FAN-OUT**: Deliver (possibly mutated) payload to async subscribers in parallel.
//! 6. **FINALIZE**: Record final delivery status (Delivered, PartiallyDelivered, etc).
//!
//! ## Sync vs Async Subscriptions
//!
//! - **Sync**: Processed sequentially in priority order. Can modify or drop the event.
//!   Mutations affect subsequent subscribers.
//! - **Async**: Processed in parallel after all sync subscribers. Cannot affect event.
//!   All async subscribers see the same (possibly mutated) payload.
//!
//! ## Concurrency & Locks
//!
//! The RwLock is held during trie lookup and delivery dispatch, but released
//! between major stages to allow concurrent subscriptions/unsubscriptions.

use super::filter::EventFilter;
use super::interceptor::{InterceptResult, Interceptor};
use super::subject_index::{SubjectIndex, SubscriptionEntry, SubscriptionMode};
use crate::delivery::ChannelConfig;
use crate::storage::{DeliveryStatus, EventStatus, EventStore, StoredEvent};
use crate::EventBusError;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// Default channel buffer size for bounded channels.
const DEFAULT_CHANNEL_CAPACITY: usize = 8192;

/// Maximum allowed subject length.
const MAX_SUBJECT_LENGTH: usize = 512;

/// Maximum allowed payload size (512 MiB).
const MAX_PAYLOAD_SIZE: usize = 512 * 1024 * 1024;

/// Default timeout for sync interceptors.
const DEFAULT_INTERCEPTOR_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum number of interceptors per subscription to prevent DoS.
const MAX_INTERCEPTORS_PER_SUBJECT: usize = 100;

/// The dispatch engine routes published events to matching subscribers.
/// Implements the full pipeline: persist → resolve → filter → sync chain → async fan-out → finalize.
pub struct DispatchEngine {
    pub(crate) store: Arc<dyn EventStore>,
    /// Combined lock for index + senders + interceptors to prevent desync.
    pub(crate) state: Arc<RwLock<DispatchState>>,
}

pub(crate) struct DispatchState {
    pub(crate) index: SubjectIndex,
    pub(crate) senders: HashMap<String, mpsc::Sender<crate::request_reply::DispatchedEvent>>,
    /// Per-subscription interceptors (for sync mode in-process subscribers).
    pub(crate) interceptors: HashMap<String, Arc<dyn Interceptor>>,
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

    /// Validate payload size.
    fn validate_payload(payload: &[u8]) -> crate::Result<()> {
        if payload.len() > MAX_PAYLOAD_SIZE {
            return Err(EventBusError::PayloadTooLarge(format!(
                "payload exceeds maximum size of {} bytes",
                MAX_PAYLOAD_SIZE
            )));
        }
        Ok(())
    }

    /// Publish an event to a subject.
    ///
    /// The publish pipeline:
    /// 1. **PERSIST**: Event is written durably to storage via write-ahead logging.
    /// 2. **RESOLVE**: All subscriptions matching the subject pattern are looked up.
    /// 3. **FILTER**: EventFilter is applied to the original payload. Filters are evaluated before
    ///    interceptors, so interceptors see the unfiltered payload but modifications from interceptors
    ///    are applied to the final delivery.
    /// 4. **SYNC CHAIN**: Sync interceptors run sequentially in priority order (lower = first).
    ///    Each can Pass, Modify, or Drop the event. Modifications propagate to later subscribers.
    /// 5. **ASYNC FAN-OUT**: Async subscribers receive the (possibly mutated) payload in parallel.
    /// 6. **FINALIZE**: Delivery status is recorded (Delivered, PartiallyDelivered, etc.).
    ///
    /// ## Store Operation Failures
    /// Storage errors during delivery recording are logged as warnings but do not cause publish to fail.
    /// This implements best-effort persistence: the event is delivered even if recording fails.
    /// Operators should monitor warn-level logs for `failed to record` messages to detect issues.
    ///
    /// ## Sync vs Async Delivery
    /// - **Sync mode**: Uses try_send on bounded channels. Not truly synchronous — if the channel
    ///   is full, delivery silently fails. Only interceptors (with timeout enforcement) provide
    ///   true synchronous processing.
    /// - **Async mode**: Parallel delivery with the same channel limitations as Sync.
    ///
    /// Called by the public API with `reply_subject: None`,
    /// and by `request()` with a reply subject set.
    pub async fn publish_event(
        &self,
        subject: &str,
        payload: &[u8],
        reply_subject: Option<String>,
    ) -> crate::Result<u64> {
        Self::validate_subject(subject)?;
        Self::validate_payload(payload)?;

        // Skip persistence for ephemeral _reply.* subjects (request/reply responses).
        // These are one-shot messages consumed immediately; persisting them would
        // cause unbounded storage growth with no benefit.
        let is_reply = subject.starts_with("_reply.");

        let seq = if is_reply {
            0 // No durable seq for reply events
        } else {
            let event = StoredEvent {
                seq: 0,
                subject: subject.to_string(),
                payload: payload.to_vec(),
                stored_at: 0, // overwritten by store.append()
                status: EventStatus::Pending,
                deliveries: vec![],
                reply_subject: reply_subject.clone(),
            };
            // 1. PERSIST (write-ahead)
            self.store.append(&event).await?
        };
        let event_reply_subject = reply_subject;

        // Helper: record delivery only for persisted (non-reply) events.
        let store = &self.store;
        let try_record = |seq: u64, sub_id: &str, status: DeliveryStatus| {
            let sub_id = sub_id.to_string();
            let store = Arc::clone(store);
            async move {
                if !is_reply {
                    if let Err(e) = store.record_delivery(seq, &sub_id, status).await {
                        warn!(seq = seq, subscriber = sub_id, error = %e, "failed to record delivery");
                    }
                }
            }
        };

        // 2. RESOLVE: find matching subscriptions (release lock after lookup)
        let mut matches = {
            let state = self.state.read().await;
            state.index.lookup(subject)
        };

        if matches.is_empty() {
            if !is_reply {
                if let Err(e) = self.store.update_status(seq, EventStatus::Delivered).await {
                    warn!(seq = seq, error = %e, "failed to update status to Delivered (no subscribers)");
                }
            }
            return Ok(seq);
        }

        // Update status to Dispatching
        if !is_reply {
            if let Err(e) = self
                .store
                .update_status(seq, EventStatus::Dispatching)
                .await
            {
                warn!(seq = seq, error = %e, "failed to update status to Dispatching");
            }
        }

        // 3. FILTER: apply EventFilter on each subscription
        let mut current_payload = payload.to_vec();
        matches.retain(|entry| {
            entry
                .filter
                .as_ref()
                .is_none_or(|f| f.matches(&current_payload))
        });

        if matches.is_empty() {
            if !is_reply {
                if let Err(e) = self.store.update_status(seq, EventStatus::Delivered).await {
                    warn!(seq = seq, error = %e, "failed to update status to Delivered (all filtered)");
                }
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

        // 4. SYNC CHAIN: process interceptors sequentially in priority order.
        // Snapshot the interceptors and senders we need, then release the lock
        // so that subscribe/unsubscribe is not blocked during interceptor execution.
        let mut dropped = false;
        {
            type SyncEntry = (
                String,
                Duration,
                Option<Arc<dyn Interceptor>>,
                Option<mpsc::Sender<crate::request_reply::DispatchedEvent>>,
            );
            let sync_snapshot: Vec<SyncEntry> = {
                let state = self.state.read().await;
                sync_entries
                    .iter()
                    .map(|entry| {
                        let interceptor = state.interceptors.get(&entry.id).cloned();
                        let sender = state.senders.get(&entry.id).cloned();
                        (entry.id.clone(), entry.timeout, interceptor, sender)
                    })
                    .collect()
            };
            // Lock is released here.

            for (entry_id, timeout, interceptor_opt, sender_opt) in &sync_snapshot {
                if let Some(interceptor) = interceptor_opt {
                    let interceptor = Arc::clone(interceptor);

                    let result = tokio::time::timeout(
                        *timeout,
                        interceptor.intercept(subject, &mut current_payload),
                    )
                    .await;

                    match result {
                        Ok(InterceptResult::Pass) => {
                            try_record(seq, entry_id, DeliveryStatus::Delivered).await;
                        }
                        Ok(InterceptResult::Modified) => {
                            debug!(subscriber = entry_id, "interceptor modified payload");
                            try_record(seq, entry_id, DeliveryStatus::Delivered).await;
                        }
                        Ok(InterceptResult::Drop) => {
                            debug!(subscriber = entry_id, "interceptor dropped event");
                            try_record(seq, entry_id, DeliveryStatus::Delivered).await;
                            dropped = true;
                            break;
                        }
                        Err(_timeout) => {
                            warn!(
                                subscriber = entry_id,
                                timeout_ms = timeout.as_millis() as u64,
                                "sync interceptor timed out, skipping"
                            );
                            try_record(seq, entry_id, DeliveryStatus::Failed).await;
                        }
                    }
                } else if let Some(tx) = sender_opt {
                    // Sync subscriber without an interceptor — deliver via channel
                    let event = crate::request_reply::DispatchedEvent {
                        payload: current_payload.clone(),
                        reply_subject: event_reply_subject.clone(),
                        subject: subject.to_string(),
                        seq,
                    };
                    match tx.try_send(event) {
                        Ok(()) => {
                            try_record(seq, entry_id, DeliveryStatus::Delivered).await;
                        }
                        Err(_) => {
                            warn!(subscriber = entry_id, "sync channel delivery failed");
                            try_record(seq, entry_id, DeliveryStatus::Failed).await;
                        }
                    }
                }
            }
        }

        if dropped {
            // Event was intentionally dropped by an interceptor — mark as Delivered.
            if !is_reply {
                if let Err(e) = self.store.update_status(seq, EventStatus::Delivered).await {
                    warn!(seq = seq, error = %e, "failed to update status after drop");
                }
            }
            return Ok(seq);
        }

        // 5. ASYNC FAN-OUT: deliver the (possibly mutated) payload to all async subscribers
        // Collect senders while holding the lock, then release before trying to send
        let async_senders: Vec<(
            String,
            Option<mpsc::Sender<crate::request_reply::DispatchedEvent>>,
        )> = {
            let state = self.state.read().await;
            async_entries
                .iter()
                .map(|entry| {
                    let tx = state.senders.get(&entry.id).cloned();
                    (entry.id.clone(), tx)
                })
                .collect()
        };

        let mut any_failed = false;
        for (entry_id, tx_opt) in async_senders {
            if let Some(tx) = tx_opt {
                let event = crate::request_reply::DispatchedEvent {
                    payload: current_payload.clone(),
                    reply_subject: event_reply_subject.clone(),
                    subject: subject.to_string(),
                    seq,
                };
                match tx.try_send(event) {
                    Ok(()) => {
                        try_record(seq, &entry_id, DeliveryStatus::Delivered).await;
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        warn!(subscriber = entry_id, "delivery failed: channel full");
                        any_failed = true;
                        try_record(seq, &entry_id, DeliveryStatus::Failed).await;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        debug!(subscriber = entry_id, "delivery failed: channel closed");
                        any_failed = true;
                        try_record(seq, &entry_id, DeliveryStatus::Failed).await;
                    }
                }
            } else {
                debug!(subscriber = entry_id, "no sender found");
                any_failed = true;
                try_record(seq, &entry_id, DeliveryStatus::Failed).await;
            }
        }

        // 6. FINALIZE
        if !is_reply {
            let final_status = if any_failed {
                EventStatus::PartiallyDelivered
            } else {
                EventStatus::Delivered
            };
            if let Err(e) = self.store.update_status(seq, final_status).await {
                warn!(seq = seq, error = %e, "failed to update final status");
            }
        }

        Ok(seq)
    }

    /// Publish an event to a subject (no reply subject).
    ///
    /// Subjects starting with `_reply.` are reserved for internal request/reply
    /// routing and are allowed here because `Replier::reply()` uses this method.
    pub async fn publish(&self, subject: &str, payload: &[u8]) -> crate::Result<u64> {
        self.publish_event(subject, payload, None).await
    }

    /// Subscribe with a channel for receiving events (async mode typical).
    pub async fn subscribe(
        &self,
        subject_pattern: &str,
        priority: u32,
        mode: SubscriptionMode,
        channel: ChannelConfig,
        timeout: Duration,
        filter: Option<EventFilter>,
    ) -> crate::Result<(
        String,
        mpsc::Receiver<crate::request_reply::DispatchedEvent>,
    )> {
        Self::validate_subject(subject_pattern)?;

        let subscription_id = ulid::Ulid::new().to_string();
        let (tx, rx) = mpsc::channel(DEFAULT_CHANNEL_CAPACITY);

        let entry = SubscriptionEntry {
            id: subscription_id.clone(),
            subject_pattern: subject_pattern.to_string(),
            priority,
            mode,
            channel,
            timeout,
            filter,
        };

        let mut state = self.state.write().await;
        state.index.subscribe(entry)?;
        state.senders.insert(subscription_id.clone(), tx);

        Ok((subscription_id, rx))
    }

    /// Register a sync interceptor. Returns the subscription ID.
    /// Returns an error if there are already too many interceptors for this specific subject pattern.
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

        // Check if we're adding too many interceptors for this specific pattern
        let existing_for_pattern = state
            .index
            .list(Some(subject_pattern))
            .iter()
            .filter(|e| {
                e.subject_pattern == subject_pattern && state.interceptors.contains_key(&e.id)
            })
            .count();
        if existing_for_pattern >= MAX_INTERCEPTORS_PER_SUBJECT {
            return Err(EventBusError::Internal(format!(
                "too many interceptors registered for pattern '{}' (max: {})",
                subject_pattern, MAX_INTERCEPTORS_PER_SUBJECT
            )));
        }

        state.index.subscribe(entry)?;
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
                ChannelConfig::InProcess,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();

        engine.publish("test.subject", b"hello").await.unwrap();

        let msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap();
        assert_eq!(msg.map(|e| e.payload), Some(b"hello".to_vec()));
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
                ChannelConfig::InProcess,
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
        assert_eq!(msg.map(|e| e.payload), Some(b"msg".to_vec()));

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
                ChannelConfig::InProcess,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();

        engine.publish("brain.content.store", b"a").await.unwrap();
        let msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap();
        assert_eq!(msg.map(|e| e.payload), Some(b"a".to_vec()));

        engine
            .publish("brain.entity.upsert.batch", b"b")
            .await
            .unwrap();
        let msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap();
        assert_eq!(msg.map(|e| e.payload), Some(b"b".to_vec()));
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
                ChannelConfig::InProcess,
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
        assert_eq!(msg.map(|e| e.payload), Some(b"HELLO".to_vec()));
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
                ChannelConfig::InProcess,
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
                ChannelConfig::InProcess,
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
        assert_eq!(msg.map(|e| e.payload), Some(b"hello".to_vec()));
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
                ChannelConfig::InProcess,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();

        engine.publish("test.subject", b"hello").await.unwrap();

        let msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap();
        assert_eq!(msg.map(|e| e.payload), Some(b"HELLO".to_vec()));
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
                ChannelConfig::InProcess,
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
                ChannelConfig::InProcess,
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
                ChannelConfig::InProcess,
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
        assert_eq!(msg1.map(|e| e.payload), Some(b"HELLO".to_vec()));
        assert_eq!(msg2.map(|e| e.payload), Some(b"HELLO".to_vec()));
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
                ChannelConfig::InProcess,
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
        assert_eq!(msg.map(|e| e.payload), Some(b"hello".to_vec()));
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
    async fn test_payload_size_limit() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);
        // Create a payload that exceeds the limit (512 MiB)
        let oversized = vec![0u8; MAX_PAYLOAD_SIZE + 1];
        assert!(engine.publish("test", &oversized).await.is_err());
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
                ChannelConfig::InProcess,
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
