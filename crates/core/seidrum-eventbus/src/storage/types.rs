use serde::{Deserialize, Serialize};

/// An event as it is stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEvent {
    /// Monotonically increasing sequence number, assigned by the store.
    pub seq: u64,
    /// The subject this event was published to.
    pub subject: String,
    /// Serialized event payload (opaque bytes to the store).
    pub payload: Vec<u8>,
    /// Timestamp of when the event was persisted (unix milliseconds).
    pub stored_at: u64,
    /// Current lifecycle status.
    pub status: EventStatus,
    /// Per-subscriber delivery tracking.
    pub deliveries: Vec<DeliveryRecord>,
    /// If this is a request, the reply subject to respond on.
    pub reply_subject: Option<String>,
}

/// Lifecycle status of an event in the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventStatus {
    /// Written to storage, not yet dispatched.
    Pending,
    /// Currently in the interceptor chain.
    Dispatching,
    /// All subscribers confirmed delivery.
    Delivered,
    /// Some subscribers failed, retry pending.
    PartiallyDelivered,
    /// Retries exhausted, moved to dead letter.
    DeadLettered,
}

impl EventStatus {
    /// Convert status to a u8 for indexing.
    pub fn as_u8(self) -> u8 {
        match self {
            EventStatus::Pending => 0,
            EventStatus::Dispatching => 1,
            EventStatus::Delivered => 2,
            EventStatus::PartiallyDelivered => 3,
            EventStatus::DeadLettered => 4,
        }
    }

    /// Convert u8 back to status.
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(EventStatus::Pending),
            1 => Some(EventStatus::Dispatching),
            2 => Some(EventStatus::Delivered),
            3 => Some(EventStatus::PartiallyDelivered),
            4 => Some(EventStatus::DeadLettered),
            _ => None,
        }
    }
}

/// Delivery status for one subscriber receiving an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryStatus {
    Pending,
    Delivered,
    Failed,
    DeadLettered,
}

/// Record of delivery to one subscriber.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryRecord {
    pub subscriber_id: String,
    pub status: DeliveryStatus,
    pub attempts: u32,
    pub last_attempt: Option<u64>, // unix milliseconds
    pub next_retry: Option<u64>,   // unix milliseconds
    pub error: Option<String>,
}

impl DeliveryRecord {
    /// Returns `true` if this delivery is currently eligible for a retry
    /// attempt: status is `Failed`, `attempts` is below the cap, and the
    /// `next_retry` schedule has elapsed (or is unset).
    ///
    /// Single source of truth used by both `query_retryable` and
    /// `count_retryable` in every store implementation.
    pub fn is_retryable(&self, max_attempts: u32, now_ms: u64) -> bool {
        self.status == DeliveryStatus::Failed
            && self.attempts < max_attempts
            && self.next_retry.is_none_or(|t| t <= now_ms)
    }
}

/// A delivery that is ready to be retried.
#[derive(Debug, Clone)]
pub struct RetryableDelivery {
    pub seq: u64,
    pub subject: String,
    pub subscriber_id: String,
    pub attempts: u32,
    pub payload: Vec<u8>,
    /// If the original event was a request, the subject to reply on.
    /// Propagated through retry so handlers receive a working `Replier`.
    pub reply_subject: Option<String>,
    /// Earliest timestamp (unix-millis) at which this delivery is eligible
    /// for the next retry attempt. Used by `query_retryable` to sort
    /// results so the earliest-due deliveries are returned first.
    pub next_retry: Option<u64>,
}

/// A persisted subscription that survives restart.
///
/// Used by the HTTP transport's webhook subscription persistence: when a
/// client creates a webhook subscription via `POST /subscribe`, the HTTP
/// server stores a `PersistedSubscription` so the subscription is recreated
/// after a process restart. The persisted entry is keyed by an internal
/// `persisted_id` (a ULID); the HTTP API exposes the runtime bus
/// subscription ID, and the server maintains a mapping from runtime ID to
/// persisted ID for unsubscribe lookups.
///
/// In-process subscriptions are not persisted — only durable transports
/// (webhooks) where the bus knows how to deliver after restart use this.
///
/// **⚠ Plaintext storage (H4):** the `headers` map is stored in clear text
/// on disk. Operators routinely set `Authorization: Bearer …` tokens
/// here. The redb backend enforces file mode `0600` on Unix to limit
/// exposure to the process owner, but anyone who can read the redb file
/// (process under the same UID, backup tarballs, container snapshots)
/// can recover the tokens. Encrypt header values at the application
/// layer before passing them to `POST /subscribe` if this matters for
/// your threat model.
/// What kind of bus binding a [`PersistedSubscription`] represents.
///
/// `AsyncWebhook` (default for compatibility) is the original subscription
/// type — the bus delivers each matching event to the URL via HTTP POST
/// and ignores the response body.
///
/// `SyncInterceptor` (C3 — Phase 4 D6 webhook half) is a sync interceptor:
/// the bus POSTs the event to the URL and parses the response body as a
/// `{"action": "pass" | "modify" | "drop", "payload": "<b64>?"}` JSON
/// document. The action drives the in-process interceptor chain just
/// like a `WsRemoteInterceptor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PersistedSubscriptionKind {
    /// Original async webhook (fire-and-forget). Default so existing
    /// persisted entries from before the C3 PR continue to deserialize.
    #[default]
    AsyncWebhook,
    /// Sync interceptor delivered via webhook POST.
    SyncInterceptor,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSubscription {
    /// Stable identifier for the persisted entry. Different from the
    /// runtime bus subscription ID (which changes across restarts).
    pub persisted_id: String,
    /// Subject pattern.
    pub pattern: String,
    /// Webhook URL.
    pub url: String,
    /// Custom HTTP headers to send with each delivery.
    pub headers: std::collections::HashMap<String, String>,
    /// Subscription priority.
    pub priority: u32,
    /// Unix-millis timestamp of when this entry was first persisted.
    pub created_at: u64,
    /// Whether this entry is an async webhook or a sync interceptor.
    /// Defaults to `AsyncWebhook` for backwards compatibility with
    /// entries persisted before the C3 PR landed.
    #[serde(default)]
    pub kind: PersistedSubscriptionKind,
    /// Per-call timeout in milliseconds. Only meaningful for
    /// `SyncInterceptor` entries — async webhooks use the bus
    /// dispatch defaults. `None` means "use the engine default at
    /// recreate time" (subject to remote-interceptor clamping).
    /// Persisted so an operator's chosen timeout survives restart.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}
