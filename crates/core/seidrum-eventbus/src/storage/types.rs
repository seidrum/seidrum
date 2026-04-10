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
