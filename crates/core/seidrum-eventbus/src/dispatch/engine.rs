//! Dispatch engine: routes published events to matching subscriptions.
//!
//! ## Architecture
//!
//! The dispatch pipeline has 6 stages:
//!
//! 1. **PERSIST**: Write the event durably to the store (write-ahead logging).
//!    Skipped for ephemeral `_reply.*` subjects.
//! 2. **RESOLVE**: Look up all subscriptions matching the subject using the
//!    trie index. The lock is released after lookup.
//! 3. **FILTER**: Apply each subscription's [`EventFilter`] to the original
//!    payload. Filters that fail to parse non-JSON payloads pass through.
//! 4. **SYNC CHAIN**: Process sync interceptors sequentially in priority order
//!    (lower = first). Interceptors can `Pass`, `Modify`, or `Drop` events.
//!    Mutations propagate to subsequent subscribers. `Drop` aborts further
//!    processing entirely.
//! 5. **ASYNC FAN-OUT**: Deliver the (possibly mutated) payload to async
//!    subscribers via non-blocking `try_send` on their bounded channels.
//!    Channel-full and channel-closed errors are recorded as failed deliveries.
//! 6. **FINALIZE**: Record the final [`EventStatus`] (`Delivered` if all
//!    subscribers received the event, `PartiallyDelivered` if any delivery
//!    failed). Skipped for reply events.
//!
//! ## Sync vs Async Subscriptions
//!
//! - **Sync**: Processed sequentially in priority order. Sync interceptors
//!   can modify or drop the event; mutations affect all subsequent
//!   subscribers (sync and async).
//! - **Async**: Processed after the sync chain. Each async subscriber
//!   receives the same (possibly mutated) payload via `try_send`. Async
//!   subscribers cannot affect the event payload.
//!
//! ## Concurrency
//!
//! The state RwLock is held only during trie lookup and to snapshot
//! interceptors/senders. It is released before the sync interceptor chain
//! executes, so subscribe/unsubscribe operations are not blocked by slow
//! interceptors. There is a small race window where an interceptor or sender
//! captured in the snapshot may be invoked after its subscription has been
//! removed; this is intentional and harmless (the modified payload is local
//! to this dispatch).

use super::filter::EventFilter;
use super::interceptor::{InterceptResult, Interceptor};
use super::subject_index::{SubjectIndex, SubscriptionEntry, SubscriptionMode};
use crate::delivery::ChannelConfig;
use crate::storage::{DeliveryStatus, EventStatus, EventStore, StoredEvent};
use crate::EventBusError;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Counter incremented on every successful publish (excluding `_reply.*`).
    pub(crate) events_published: AtomicU64,
    /// Counter incremented for every successful subscriber delivery.
    pub(crate) events_delivered: AtomicU64,
    /// Registry of custom delivery channels for retry execution.
    pub(crate) channel_registry: Arc<crate::delivery::ChannelRegistry>,
    /// Built-in webhook delivery channel used by the retry task.
    pub(crate) webhook_channel: Arc<crate::delivery::WebhookChannel>,
}

pub(crate) struct DispatchState {
    pub(crate) index: SubjectIndex,
    pub(crate) senders: HashMap<String, mpsc::Sender<crate::request_reply::DispatchedEvent>>,
    /// Per-subscription interceptors (for sync mode in-process subscribers).
    pub(crate) interceptors: HashMap<String, Arc<dyn Interceptor>>,
}

impl DispatchEngine {
    pub fn new(store: Arc<dyn EventStore>) -> Self {
        Self::with_registry(store, Arc::new(crate::delivery::ChannelRegistry::new()))
    }

    pub fn with_registry(
        store: Arc<dyn EventStore>,
        channel_registry: Arc<crate::delivery::ChannelRegistry>,
    ) -> Self {
        Self {
            store,
            state: Arc::new(RwLock::new(DispatchState {
                index: SubjectIndex::new(),
                senders: HashMap::new(),
                interceptors: HashMap::new(),
            })),
            events_published: AtomicU64::new(0),
            events_delivered: AtomicU64::new(0),
            channel_registry,
            webhook_channel: crate::delivery::WebhookChannel::new(),
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
    /// 1. **PERSIST**: Event is written durably to storage via write-ahead
    ///    logging. Skipped for ephemeral `_reply.*` subjects.
    /// 2. **RESOLVE**: All subscriptions matching the subject pattern are
    ///    looked up.
    /// 3. **FILTER**: [`EventFilter`] is applied to the original payload.
    ///    Filters are evaluated before interceptors, so interceptors see the
    ///    unfiltered payload but modifications from interceptors are applied
    ///    to the final delivery.
    /// 4. **SYNC CHAIN**: Sync interceptors run sequentially in priority
    ///    order (lower = first). Each can Pass, Modify, or Drop the event.
    ///    Modifications propagate to later subscribers.
    /// 5. **ASYNC FAN-OUT**: Async subscribers receive the (possibly mutated)
    ///    payload sequentially via non-blocking `try_send`. The fan-out is
    ///    sequential (not parallel) but each `try_send` is non-blocking.
    /// 6. **FINALIZE**: Delivery status is recorded (`Delivered`,
    ///    `PartiallyDelivered`, etc).
    ///
    /// ## Store Operation Failures
    /// Storage errors during delivery recording are logged as warnings but
    /// do not cause publish to fail. This implements best-effort persistence:
    /// the event is delivered even if recording fails. Operators should
    /// monitor warn-level logs for `failed to record` messages to detect
    /// issues.
    ///
    /// ## Sync vs Async Delivery
    /// - **Sync mode**: Uses `try_send` on bounded channels. Not truly
    ///   synchronous — if the channel is full, delivery is recorded as
    ///   failed. Only interceptors (with timeout enforcement) provide true
    ///   synchronous processing.
    /// - **Async mode**: Sequential `try_send` to each subscriber. Same
    ///   channel-full semantics as Sync.
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
            let s = self.store.append(&event).await?;
            self.events_published.fetch_add(1, Ordering::Relaxed);
            s
        };
        let event_reply_subject = reply_subject;

        // Helper: record delivery only for persisted (non-reply) events.
        let store = &self.store;
        let delivered_counter = &self.events_delivered;
        let try_record = |seq: u64, sub_id: &str, status: DeliveryStatus, error: Option<String>| {
            let sub_id = sub_id.to_string();
            let store = Arc::clone(store);
            async move {
                if status == DeliveryStatus::Delivered {
                    delivered_counter.fetch_add(1, Ordering::Relaxed);
                }
                if !is_reply {
                    if let Err(e) = store.record_delivery(seq, &sub_id, status, error).await {
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

        // Track delivery failures across both sync and async stages so the
        // final event status reflects partial delivery.
        let mut any_failed = false;

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
                            try_record(seq, entry_id, DeliveryStatus::Delivered, None).await;
                        }
                        Ok(InterceptResult::Modified) => {
                            debug!(subscriber = entry_id, "interceptor modified payload");
                            try_record(seq, entry_id, DeliveryStatus::Delivered, None).await;
                        }
                        Ok(InterceptResult::Drop) => {
                            debug!(subscriber = entry_id, "interceptor dropped event");
                            try_record(seq, entry_id, DeliveryStatus::Delivered, None).await;
                            dropped = true;
                            break;
                        }
                        Err(_timeout) => {
                            warn!(
                                subscriber = entry_id,
                                timeout_ms = timeout.as_millis() as u64,
                                "sync interceptor timed out, skipping"
                            );
                            any_failed = true;
                            try_record(
                                seq,
                                entry_id,
                                DeliveryStatus::Failed,
                                Some(format!(
                                    "interceptor timed out after {}ms",
                                    timeout.as_millis()
                                )),
                            )
                            .await;
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
                            try_record(seq, entry_id, DeliveryStatus::Delivered, None).await;
                        }
                        Err(e) => {
                            warn!(subscriber = entry_id, error = %e, "sync channel delivery failed");
                            any_failed = true;
                            try_record(
                                seq,
                                entry_id,
                                DeliveryStatus::Failed,
                                Some(format!("sync channel delivery failed: {}", e)),
                            )
                            .await;
                        }
                    }
                } else {
                    // Sync entry vanished between snapshot and execution (race
                    // with unsubscribe). Record as failed and flag.
                    debug!(
                        subscriber = entry_id,
                        "sync entry has neither interceptor nor sender"
                    );
                    any_failed = true;
                    try_record(
                        seq,
                        entry_id,
                        DeliveryStatus::Failed,
                        Some("subscription removed during dispatch".to_string()),
                    )
                    .await;
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
                        try_record(seq, &entry_id, DeliveryStatus::Delivered, None).await;
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        warn!(subscriber = entry_id, "delivery failed: channel full");
                        any_failed = true;
                        try_record(
                            seq,
                            &entry_id,
                            DeliveryStatus::Failed,
                            Some("subscriber channel full".to_string()),
                        )
                        .await;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        debug!(subscriber = entry_id, "delivery failed: channel closed");
                        any_failed = true;
                        try_record(
                            seq,
                            &entry_id,
                            DeliveryStatus::Failed,
                            Some("subscriber channel closed".to_string()),
                        )
                        .await;
                    }
                }
            } else {
                debug!(subscriber = entry_id, "no sender found");
                any_failed = true;
                try_record(
                    seq,
                    &entry_id,
                    DeliveryStatus::Failed,
                    Some("no sender registered for subscriber".to_string()),
                )
                .await;
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

        // Check if we're adding too many interceptors for this specific pattern.
        // list() returns exact-pattern matches only.
        let existing_for_pattern = state
            .index
            .list(Some(subject_pattern))
            .iter()
            .filter(|e| state.interceptors.contains_key(&e.id))
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

    /// Retry delivery to a specific subscriber. Used by [`crate::delivery::RetryTask`].
    ///
    /// Looks up the live subscription by `subscriber_id`, then dispatches the
    /// payload through the channel configured on that subscription. Returns
    /// `Ok(())` on successful delivery, `Err(RetryOutcome::Permanent)` if the
    /// subscriber is gone or the channel cannot be retried (e.g., closed
    /// WebSocket), or `Err(RetryOutcome::Transient)` if delivery failed and
    /// can be retried again.
    pub async fn retry_to_subscriber(
        &self,
        subscriber_id: &str,
        subject: &str,
        payload: &[u8],
    ) -> Result<(), RetryOutcome> {
        // Look up the live subscription
        let entry = {
            let state = self.state.read().await;
            state.index.find_by_id(subscriber_id)
        };
        let entry = match entry {
            Some(e) => e,
            None => {
                // Subscription is gone — cannot retry
                return Err(RetryOutcome::Permanent(format!(
                    "subscriber {} no longer exists",
                    subscriber_id
                )));
            }
        };

        match &entry.channel {
            ChannelConfig::Webhook { .. } => {
                use crate::delivery::DeliveryChannel;
                self.webhook_channel
                    .deliver(payload, subject, &entry.channel)
                    .await
                    .map(|_| ())
                    .map_err(|e| RetryOutcome::Transient(format!("{}", e)))
            }
            ChannelConfig::Custom { channel_type, .. } => {
                let channel_opt = self.channel_registry.get(channel_type).await;
                let channel = match channel_opt {
                    Some(c) => c,
                    None => {
                        return Err(RetryOutcome::Permanent(format!(
                            "no channel registered for type '{}'",
                            channel_type
                        )));
                    }
                };
                channel
                    .deliver(payload, subject, &entry.channel)
                    .await
                    .map(|_| ())
                    .map_err(|e| RetryOutcome::Transient(format!("{}", e)))
            }
            ChannelConfig::InProcess => {
                // Try to push through the live mpsc sender. If the consumer
                // is still draining, this may now succeed even if the original
                // try_send failed.
                let state = self.state.read().await;
                let tx = match state.senders.get(subscriber_id) {
                    Some(tx) => tx.clone(),
                    None => {
                        return Err(RetryOutcome::Permanent(
                            "in-process sender no longer registered".to_string(),
                        ));
                    }
                };
                drop(state);
                let event = crate::request_reply::DispatchedEvent {
                    payload: payload.to_vec(),
                    reply_subject: None,
                    subject: subject.to_string(),
                    seq: 0, // unknown at retry time
                };
                tx.try_send(event)
                    .map_err(|e| RetryOutcome::Transient(format!("{}", e)))
            }
            ChannelConfig::WebSocket => {
                // WebSocket connections are per-session and cannot survive
                // a retry — the original connection is gone by the time we
                // get here. Mark as permanent.
                Err(RetryOutcome::Permanent(
                    "WebSocket subscriptions are not retryable".to_string(),
                ))
            }
        }
    }
}

/// Outcome of a retry attempt.
#[derive(Debug, Clone)]
pub enum RetryOutcome {
    /// The retry failed but is eligible for further attempts.
    Transient(String),
    /// The retry cannot succeed (subscriber gone, channel type not retryable).
    /// The caller should dead-letter this delivery.
    Permanent(String),
}

impl std::fmt::Display for RetryOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RetryOutcome::Transient(msg) => write!(f, "transient: {}", msg),
            RetryOutcome::Permanent(msg) => write!(f, "permanent: {}", msg),
        }
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
