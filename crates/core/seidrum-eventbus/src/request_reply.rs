//! Request/reply pattern implementation for the event bus.
//!
//! This module implements request/reply semantics on top of the pub/sub engine.
//! When a caller publishes a request:
//!
//! 1. A unique reply subject is generated (e.g., `_reply.{ulid}`)
//! 2. A one-shot subscription is created on the reply subject
//! 3. The event is published with `reply_subject` set
//! 4. Subscribers receive the event and can reply via the `Replier` handle
//! 5. The one-shot subscription receives the reply and returns it to the caller
//! 6. On timeout, the request returns `Err(RequestTimeout)`
//!
//! The `serve()` function registers a request handler that receives events
//! bundled with a `Replier` for sending responses.

use crate::dispatch::DispatchEngine;
use crate::EventBusError;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// An event as dispatched to subscribers, including metadata needed for request/reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchedEvent {
    /// The raw payload bytes.
    pub payload: Vec<u8>,
    /// If this event is a request, the subject to reply on.
    pub reply_subject: Option<String>,
    /// The subject the event was published to.
    pub subject: String,
    /// The sequence number assigned by storage.
    pub seq: u64,
}

/// A handle for sending a reply to a request.
pub struct Replier {
    engine: Arc<DispatchEngine>,
    reply_subject: String,
}

impl Replier {
    /// Send a reply to the requester.
    pub async fn reply(&self, payload: &[u8]) -> crate::Result<u64> {
        self.engine.publish(&self.reply_subject, payload).await
    }
}

impl std::fmt::Debug for Replier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Replier")
            .field("reply_subject", &self.reply_subject)
            .finish()
    }
}

/// A message received via serve(), including the payload and metadata.
#[derive(Debug, Clone)]
pub struct RequestMessage {
    /// The request payload.
    pub payload: Vec<u8>,
    /// The subject the request was published to.
    pub subject: String,
    /// The sequence number of the request.
    pub seq: u64,
}

/// A subscription for handling requests.
///
/// When you call serve(), you get a RequestSubscription that allows you to
/// receive incoming requests and send replies. Drop it to unsubscribe.
pub struct RequestSubscription {
    /// The subscription ID (for cleanup).
    pub id: String,
    /// Channel for receiving request messages with repliers.
    pub rx: mpsc::Receiver<(RequestMessage, Replier)>,
}

impl Drop for RequestSubscription {
    fn drop(&mut self) {
        debug!(subscription_id = %self.id, "RequestSubscription dropped");
    }
}

impl DispatchEngine {
    /// Send a request to a subject and wait for a reply.
    ///
    /// Generates a unique `_reply.{ulid}` subject, subscribes to it,
    /// publishes the request with `reply_subject` set, waits for the reply,
    /// and cleans up the one-shot subscription.
    ///
    /// Returns the reply payload, or `Err(RequestTimeout)` if no reply arrives
    /// within the timeout duration.
    pub async fn request(
        &self,
        subject: &str,
        payload: &[u8],
        timeout: Duration,
    ) -> crate::Result<Vec<u8>> {
        let reply_subject = format!("_reply.{}", ulid::Ulid::new());
        debug!(subject = %subject, reply_subject = %reply_subject, "publishing request");

        // Create a one-shot subscription on the reply subject (capacity 1).
        // Subscribe BEFORE publishing to avoid race conditions.
        let (reply_tx, mut reply_rx) = mpsc::channel::<DispatchedEvent>(1);
        let reply_sub_id = ulid::Ulid::new().to_string();
        {
            let mut state = self.state.write().await;
            state.senders.insert(reply_sub_id.clone(), reply_tx);

            // Register in trie for exact match on reply subject
            let entry = crate::dispatch::subject_index::SubscriptionEntry {
                id: reply_sub_id.clone(),
                subject_pattern: reply_subject.clone(),
                priority: 0,
                mode: crate::dispatch::SubscriptionMode::Async,
                channel: crate::delivery::ChannelConfig::InProcess,
                timeout: Duration::from_secs(30),
                filter: None,
            };
            state.index.subscribe(entry)?;
        }

        // Publish the request with reply_subject set.
        let publish_result = self
            .publish_event(subject, payload, Some(reply_subject.clone()))
            .await;

        // If publish failed, clean up and return error
        if let Err(e) = publish_result {
            self.unsubscribe(&reply_sub_id).await.ok();
            return Err(e);
        }

        // Wait for the reply with timeout.
        let result = tokio::time::timeout(timeout, reply_rx.recv()).await;

        // Close the receiver first to prevent any in-flight replies from being
        // lost between timeout and unsubscribe, then clean up the trie entry.
        drop(reply_rx);
        self.unsubscribe(&reply_sub_id).await.ok();

        match result {
            Ok(Some(event)) => {
                debug!(subject = %subject, "received reply");
                Ok(event.payload)
            }
            Ok(None) => {
                warn!(subject = %subject, "reply channel closed without message");
                Err(EventBusError::RequestTimeout)
            }
            Err(_timeout) => {
                warn!(subject = %subject, "request timed out");
                Err(EventBusError::RequestTimeout)
            }
        }
    }

    /// Register a request handler on a subject.
    ///
    /// Returns a `RequestSubscription` that yields `(RequestMessage, Replier)` pairs.
    /// The handler calls `replier.reply(payload)` to send a response.
    ///
    /// Only events published with a `reply_subject` (i.e., via `request()`) are
    /// forwarded to the handler. Regular `publish()` events on the same subject
    /// are silently skipped by the bridge task.
    ///
    /// Requires `Arc<Self>` so that `Replier` can hold a reference to the engine
    /// for publishing replies.
    pub async fn serve(
        self: &Arc<Self>,
        subject_pattern: &str,
        priority: u32,
        timeout: Duration,
        filter: Option<crate::dispatch::filter::EventFilter>,
    ) -> crate::Result<RequestSubscription> {
        use crate::dispatch::SubscriptionMode;

        // Create the internal subscription to receive DispatchedEvents
        let (sub_id, mut event_rx) = self
            .subscribe(
                subject_pattern,
                priority,
                SubscriptionMode::Async,
                crate::delivery::ChannelConfig::InProcess,
                timeout,
                filter,
            )
            .await?;

        // Create the external channel for (RequestMessage, Replier) pairs
        const SERVE_CHANNEL_CAPACITY: usize = 128;
        let (req_tx, req_rx) = mpsc::channel(SERVE_CHANNEL_CAPACITY);

        let engine = Arc::clone(self);

        // Spawn a bridge task: DispatchedEvent → (RequestMessage, Replier)
        // Only forwards events that have a reply_subject (i.e., actual requests).
        // Regular publish() events without reply_subject are silently skipped —
        // they aren't requests and have no one to reply to.
        tokio::spawn(async move {
            while let Some(dispatched) = event_rx.recv().await {
                let reply_subject = match dispatched.reply_subject {
                    Some(rs) if !rs.is_empty() => rs,
                    _ => {
                        debug!(
                            subject = %dispatched.subject,
                            "serve() received non-request event (no reply_subject), skipping"
                        );
                        continue;
                    }
                };

                let req_msg = RequestMessage {
                    payload: dispatched.payload,
                    subject: dispatched.subject,
                    seq: dispatched.seq,
                };

                let replier = Replier {
                    engine: Arc::clone(&engine),
                    reply_subject,
                };

                if req_tx.send((req_msg, replier)).await.is_err() {
                    break; // RequestSubscription dropped
                }
            }
            // Bridge task exiting — drop engine Arc to allow clean shutdown.
            drop(engine);
        });

        Ok(RequestSubscription {
            id: sub_id,
            rx: req_rx,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::memory_store::InMemoryEventStore;

    #[tokio::test]
    async fn test_basic_request_reply_roundtrip() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = Arc::new(DispatchEngine::new(store));

        // Spawn a handler that echoes requests
        let handler_engine = Arc::clone(&engine);
        tokio::spawn(async move {
            let mut sub = handler_engine
                .serve("test.request", 10, Duration::from_secs(5), None)
                .await
                .unwrap();

            while let Some((req_msg, replier)) = sub.rx.recv().await {
                let response = format!("echo: {}", String::from_utf8_lossy(&req_msg.payload));
                let _ = replier.reply(response.as_bytes()).await;
            }
        });

        // Give the handler time to register
        tokio::time::sleep(Duration::from_millis(50)).await;

        let reply = engine
            .request("test.request", b"hello", Duration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(reply, b"echo: hello".to_vec());
    }

    #[tokio::test]
    async fn test_request_timeout_no_handler() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);

        let result = engine
            .request("test.request", b"hello", Duration::from_millis(100))
            .await;

        assert!(matches!(result, Err(EventBusError::RequestTimeout)));
    }

    #[tokio::test]
    async fn test_request_timeout_slow_handler() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = Arc::new(DispatchEngine::new(store));

        let handler_engine = Arc::clone(&engine);
        tokio::spawn(async move {
            let mut sub = handler_engine
                .serve("test.request", 10, Duration::from_secs(5), None)
                .await
                .unwrap();

            while let Some((_, replier)) = sub.rx.recv().await {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let _ = replier.reply(b"late reply").await;
            }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let result = engine
            .request("test.request", b"hello", Duration::from_millis(100))
            .await;

        assert!(matches!(result, Err(EventBusError::RequestTimeout)));
    }

    #[tokio::test]
    async fn test_concurrent_requests_same_subject() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = Arc::new(DispatchEngine::new(store));

        let handler_engine = Arc::clone(&engine);
        tokio::spawn(async move {
            let mut sub = handler_engine
                .serve("test.request", 10, Duration::from_secs(5), None)
                .await
                .unwrap();

            while let Some((req_msg, replier)) = sub.rx.recv().await {
                let response = format!("re: {}", String::from_utf8_lossy(&req_msg.payload));
                let _ = replier.reply(response.as_bytes()).await;
            }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let e1 = Arc::clone(&engine);
        let e2 = Arc::clone(&engine);
        let e3 = Arc::clone(&engine);

        let (r1, r2, r3) = tokio::join!(
            e1.request("test.request", b"req1", Duration::from_secs(1)),
            e2.request("test.request", b"req2", Duration::from_secs(1)),
            e3.request("test.request", b"req3", Duration::from_secs(1)),
        );

        assert!(r1.is_ok());
        assert!(r2.is_ok());
        assert!(r3.is_ok());
    }

    #[tokio::test]
    async fn test_request_cleanup_on_timeout() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = DispatchEngine::new(store);

        let result = engine
            .request("test.request", b"hello", Duration::from_millis(50))
            .await;

        assert!(matches!(result, Err(EventBusError::RequestTimeout)));

        // Verify the one-shot subscription was cleaned up
        let subs = engine.list_subscriptions(None).await.unwrap();
        assert!(subs.is_empty());
    }

    #[tokio::test]
    async fn test_request_through_interceptor() {
        use crate::dispatch::interceptor::{InterceptResult, Interceptor};
        use async_trait::async_trait;

        struct UppercaseInterceptor;

        #[async_trait]
        impl Interceptor for UppercaseInterceptor {
            async fn intercept(&self, _subject: &str, payload: &mut Vec<u8>) -> InterceptResult {
                payload.iter_mut().for_each(|b| *b = b.to_ascii_uppercase());
                InterceptResult::Modified
            }
        }

        let store = Arc::new(InMemoryEventStore::new());
        let engine = Arc::new(DispatchEngine::new(store));

        // Register interceptor on the request subject
        engine
            .intercept("test.request", 5, Arc::new(UppercaseInterceptor), None)
            .await
            .unwrap();

        // Handler sees uppercased payload
        let handler_engine = Arc::clone(&engine);
        tokio::spawn(async move {
            let mut sub = handler_engine
                .serve("test.request", 10, Duration::from_secs(5), None)
                .await
                .unwrap();

            while let Some((req_msg, replier)) = sub.rx.recv().await {
                // Should receive uppercased payload from interceptor
                let _ = replier.reply(&req_msg.payload).await;
            }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let reply = engine
            .request("test.request", b"hello", Duration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(reply, b"HELLO".to_vec());
    }

    #[tokio::test]
    async fn test_serve_skips_non_request_events() {
        let store = Arc::new(InMemoryEventStore::new());
        let engine = Arc::new(DispatchEngine::new(store));

        // Register a serve handler
        let mut req_sub = engine
            .serve("test.subject", 10, Duration::from_secs(5), None)
            .await
            .unwrap();

        // Publish a regular event (no reply_subject) — serve() bridge should skip it
        engine.publish("test.subject", b"regular").await.unwrap();

        // Give the bridge task time to process
        tokio::time::sleep(Duration::from_millis(50)).await;

        // The serve channel should NOT receive anything for non-request events
        let result = tokio::time::timeout(Duration::from_millis(200), req_sub.rx.recv()).await;
        assert!(
            result.is_err(),
            "serve() should skip events without reply_subject"
        );
    }
}
