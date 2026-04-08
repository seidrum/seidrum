# ARCHITECTURE.md — seidrum-eventbus Internal Architecture

## Overview

The crate is organized into four layers, each behind a trait boundary. Higher
layers depend on lower layers. Nothing depends upward. External code
(seidrum-kernel, plugins) interacts exclusively through the top-level
`EventBus` trait.

```
┌─────────────────────────────────────────────────────────────────┐
│                        EventBus trait                           │
│  publish · subscribe · request · reply · register_channel_type  │
├─────────────────────────────────────────────────────────────────┤
│                     Dispatch Engine                              │
│  subject index · interceptor chains · fan-out · timeout mgmt    │
├───────────────┬─────────────────────────────────────────────────┤
│  Delivery     │  Transport Servers                              │
│  Channels     │  (WebSocket server, HTTP server)                │
│  (trait impls)│  accept remote connections, bridge to bus       │
├───────────────┴─────────────────────────────────────────────────┤
│                     Storage Engine                               │
│  write-ahead persist · status tracking · retry index · cleanup  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Layer 1: Storage Engine

The storage engine is the foundation. It persists events durably before they
enter the dispatch pipeline and tracks per-subscriber delivery status
afterward. The engine is embedded (no external process) and accessed through
a trait so the backing implementation can be swapped.

### Storage Trait

```rust
#[async_trait]
pub trait EventStore: Send + Sync + 'static {
    /// Persist an event. Returns the assigned sequence number.
    /// This is the write-ahead step — the event is durable after
    /// this call returns.
    async fn append(&self, event: &StoredEvent) -> Result<u64>;

    /// Update the dispatch status of an event.
    async fn update_status(&self, seq: u64, status: EventStatus) -> Result<()>;

    /// Record delivery outcome for one subscriber.
    async fn record_delivery(
        &self,
        seq: u64,
        subscriber_id: &str,
        status: DeliveryStatus,
    ) -> Result<()>;

    /// Query events by status (for crash recovery and retry).
    async fn query_by_status(
        &self,
        status: EventStatus,
        limit: usize,
    ) -> Result<Vec<StoredEvent>>;

    /// Query events by subject pattern (for replay and debugging).
    async fn query_by_subject(
        &self,
        pattern: &str,
        since: Option<u64>,
        limit: usize,
    ) -> Result<Vec<StoredEvent>>;

    /// Query failed deliveries that are due for retry.
    async fn query_retryable(
        &self,
        max_attempts: u32,
        limit: usize,
    ) -> Result<Vec<RetryableDelivery>>;

    /// Compact: remove fully-delivered events older than the
    /// retention threshold.
    async fn compact(&self, older_than: Duration) -> Result<u64>;
}
```

### Stored Event Schema

```rust
pub struct StoredEvent {
    /// Monotonically increasing sequence number, assigned by the store.
    pub seq: u64,
    /// The subject this event was published to.
    pub subject: String,
    /// Serialized event payload (opaque bytes to the store).
    pub payload: Vec<u8>,
    /// Timestamp of when the event was persisted.
    pub stored_at: u64,
    /// Current lifecycle status.
    pub status: EventStatus,
    /// Per-subscriber delivery tracking.
    pub deliveries: Vec<DeliveryRecord>,
    /// If this is a request, the reply subject to respond on.
    pub reply_subject: Option<String>,
}

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

pub struct DeliveryRecord {
    pub subscriber_id: String,
    pub status: DeliveryStatus,
    pub attempts: u32,
    pub last_attempt: Option<u64>,
    pub next_retry: Option<u64>,
    pub error: Option<String>,
}

pub enum DeliveryStatus {
    Pending,
    Delivered,
    Failed,
    DeadLettered,
}
```

### Default Implementation: redb

The default `EventStore` implementation uses `redb`, a pure-Rust embedded
ACID database. Tables:

| Table | Key | Value | Purpose |
|-------|-----|-------|---------|
| `events` | `seq: u64` | `StoredEvent` | Primary event storage |
| `subject_idx` | `(subject, seq)` | `()` | Subject-based lookup |
| `status_idx` | `(status, seq)` | `()` | Recovery and retry scans |
| `delivery_idx` | `(subscriber_id, seq)` | `DeliveryStatus` | Per-subscriber tracking |

Write path: a single `redb` write transaction that inserts into `events`
and `subject_idx` atomically. This is the critical path — it must complete
in microseconds for the write-ahead guarantee to not bottleneck dispatch.

Status updates and delivery records are written in separate transactions
after dispatch completes. These are not on the hot path.

### Compaction

A background task runs on a configurable interval (default: 1 hour) and
removes events in `Delivered` status that are older than the retention
period (default: 24 hours). Dead-lettered events have a longer retention
(default: 7 days) for debugging.

---

## Layer 2: Delivery Channels and Transport Servers

### Delivery Channel Trait

A delivery channel is a strategy for getting an event from the bus to a
subscriber. The bus ships with built-in channels and supports plugin-registered
custom channels.

```rust
#[async_trait]
pub trait DeliveryChannel: Send + Sync + 'static {
    /// Deliver one event to the subscriber.
    /// Returns Ok(receipt) on success, Err on failure (triggers retry).
    async fn deliver(
        &self,
        event: &[u8],
        subject: &str,
        config: &ChannelConfig,
    ) -> Result<DeliveryReceipt>;

    /// Called when the subscription is removed or the subscriber
    /// disconnects. Clean up any connection state.
    async fn cleanup(&self, config: &ChannelConfig) -> Result<()>;

    /// Health check. Returns true if the channel is operational.
    async fn is_healthy(&self, config: &ChannelConfig) -> bool;
}

pub struct DeliveryReceipt {
    pub delivered_at: u64,
    pub latency_us: u64,
}
```

### Built-in Delivery Channels

**InProcessChannel.** Uses `tokio::mpsc` bounded channels. The subscriber
receives raw bytes with zero serialization overhead (or typed values if
the subscriber is in the same binary and opts into generic dispatch). This
is the fastest path — used by Rust plugins compiled into the kernel.

**WebSocketChannel.** Maintains a persistent connection to a remote
subscriber. Events are framed as length-prefixed binary messages. The
channel handles reconnection detection: if the WebSocket drops, deliveries
fail and enter the retry queue. When the subscriber reconnects, pending
deliveries resume.

**WebhookChannel.** Sends an HTTP POST per event to a configured URL.
Supports configurable headers (for auth tokens), configurable retry policy,
and delivery confirmation via HTTP status code (2xx = delivered). Suitable
for serverless functions and external integrations that do not maintain
persistent connections.

**CustomChannel.** A proxy that delegates to a plugin-registered
`DeliveryChannel` implementation. The bus holds a registry of
`channel_type -> Arc<dyn DeliveryChannel>`. When a subscription requests a
custom channel type, the bus looks up the provider and delegates. If the
provider plugin disconnects, deliveries to custom-channel subscribers
fail and enter the retry queue.

### Channel Configuration

```rust
pub enum ChannelConfig {
    InProcess,
    WebSocket {
        /// Connection ID assigned by the transport server.
        connection_id: String,
    },
    Webhook {
        url: String,
        headers: HashMap<String, String>,
        retry: RetryPolicy,
    },
    Custom {
        channel_type: String,
        config: serde_json::Value,
    },
}

pub struct RetryPolicy {
    /// Maximum number of delivery attempts.
    pub max_attempts: u32,
    /// Initial backoff duration.
    pub initial_backoff: Duration,
    /// Backoff multiplier per attempt.
    pub multiplier: f64,
    /// Maximum backoff duration.
    pub max_backoff: Duration,
}
```

### Transport Servers

Transport servers are the network-facing components that accept remote
connections and bridge them into the bus. They are optional — an in-process-only
deployment does not start them.

**WebSocket Server.** Built on `tokio-tungstenite`. Listens on a configurable
address. Each connection goes through an authentication handshake (API key
or JWT in the initial message or query parameter). Authenticated connections
can:

- Register as a plugin
- Subscribe to subjects (with priority, mode, and implicit WebSocket delivery)
- Publish events
- Send request/reply messages
- Register custom delivery channel types
- Respond to health checks

The protocol is JSON-framed messages with a `type` field discriminator,
similar to the current API gateway WebSocket protocol but extended with
interceptor chain semantics.

**HTTP Server.** Built on `axum`. Provides:

- `POST /publish` — publish an event
- `POST /request` — request/reply (synchronous, waits for reply)
- `POST /subscribe` — register a webhook subscription
- `DELETE /subscribe/{id}` — remove a subscription
- `GET /events/{seq}` — retrieve a stored event by sequence number
- `GET /health` — bus health check

The HTTP server is stateless per-request. It does not maintain long-lived
connections. Webhook subscriptions registered via HTTP are persisted in the
event store so they survive restarts.

---

## Layer 3: Dispatch Engine

The dispatch engine is the core event routing logic. It receives a published
event (already persisted by the storage engine), resolves matching
subscribers, executes the interceptor chain, and fans out to async
subscribers.

### Subject Index

The dispatch engine maintains an in-memory index of subscriptions, organized
by subject pattern. The index supports exact subjects and two wildcard forms:

- `*` matches exactly one token: `channel.*.inbound` matches
  `channel.telegram.inbound` but not `channel.telegram.sub.inbound`
- `>` matches one or more tokens at the end: `brain.>` matches
  `brain.content.store` and `brain.entity.upsert`

The index is a trie keyed by subject tokens. Lookup is O(depth) where depth
is the number of tokens in the published subject. Wildcard matching is
resolved during trie traversal.

```rust
struct SubjectIndex {
    root: TrieNode,
}

struct TrieNode {
    /// Subscriptions attached at this exact position.
    subscriptions: Vec<SubscriptionEntry>,
    /// Children keyed by literal token.
    children: HashMap<String, TrieNode>,
    /// Subscriptions using '*' at this position.
    wildcard: Option<Box<TrieNode>>,
    /// Subscriptions using '>' at this position (terminal wildcard).
    terminal_wildcard: Vec<SubscriptionEntry>,
}

struct SubscriptionEntry {
    pub id: String,
    pub priority: u32,
    pub mode: SubscriptionMode,
    pub channel: ChannelConfig,
    pub timeout: Duration,
    pub filter: Option<EventFilter>,
}

pub enum SubscriptionMode {
    /// Handler receives the event mutably. Processed sequentially
    /// in priority order. Can modify or drop the event.
    Sync,
    /// Handler receives a clone. Runs in parallel with other
    /// async subscribers. Cannot affect the sync chain.
    Async,
}
```

### Event Filters

Subscriptions can optionally declare a filter that narrows which events they
receive beyond subject matching. Filters operate on the serialized event
payload without deserializing into a specific type.

```rust
pub enum EventFilter {
    /// Match a JSON path against a value.
    /// Example: field "platform" equals "telegram"
    FieldEquals { path: String, value: serde_json::Value },
    /// Match a JSON path against a pattern.
    FieldContains { path: String, substring: String },
    /// Logical AND of multiple filters.
    All(Vec<EventFilter>),
    /// Logical OR of multiple filters.
    Any(Vec<EventFilter>),
}
```

### Dispatch Pipeline

When an event is published:

```
1. PERSIST
   │  Storage engine assigns seq, writes to events table + subject_idx.
   │  Status: Pending.
   │
2. RESOLVE
   │  Subject index lookup returns all matching SubscriptionEntry items.
   │  Split into two lists: sync (sorted by priority ASC) and async.
   │
3. FILTER
   │  Apply EventFilter on each subscription. Remove non-matching entries.
   │
4. SYNC CHAIN
   │  Status: Dispatching.
   │  For each sync subscriber in priority order:
   │    a. Deliver event via the subscriber's delivery channel.
   │    b. Wait for response (mutated event or drop signal).
   │    c. If timeout expires → skip, log warning, continue.
   │    d. If drop signal → abort pipeline, status: Delivered (dropped).
   │    e. If mutated → replace event payload, continue to next handler.
   │    f. Record delivery status.
   │
5. ASYNC FAN-OUT
   │  Take the final (possibly mutated) event from the sync chain.
   │  Spawn a delivery task for each async subscriber concurrently.
   │  Each task:
   │    a. Deliver via the subscriber's delivery channel.
   │    b. On success → record DeliveryStatus::Delivered.
   │    c. On failure → record DeliveryStatus::Failed, schedule retry.
   │
6. FINALIZE
   │  If all deliveries succeeded → status: Delivered.
   │  If some failed → status: PartiallyDelivered.
   │  Retry background task picks up PartiallyDelivered events.
```

### Sync Interceptor Protocol

For in-process subscribers, the sync interceptor is a Rust closure or trait
object:

```rust
#[async_trait]
pub trait Interceptor: Send + Sync + 'static {
    /// Process the event. Return Modified(bytes) to continue with
    /// changes, Pass to continue unchanged, or Drop to kill the event.
    async fn intercept(
        &self,
        subject: &str,
        payload: &mut Vec<u8>,
    ) -> InterceptResult;
}

pub enum InterceptResult {
    /// Event was modified in place. Continue the chain.
    Modified,
    /// Event passes through unchanged. Continue the chain.
    Pass,
    /// Event is dropped. Abort the chain, do not deliver to
    /// async subscribers.
    Drop,
}
```

For remote subscribers (WebSocket, webhook), the sync interceptor protocol
is a request/reply exchange: the bus sends the event, the subscriber returns
a response indicating Modified (with new payload), Pass, or Drop. The bus
enforces the timeout — if no response arrives, the interceptor is skipped.

### Request/Reply

Request/reply is built on top of pub/sub. When a caller publishes a request:

1. The bus generates a unique reply subject (e.g., `_reply.{ulid}`).
2. The bus creates a one-shot subscription on the reply subject.
3. The event is published to the target subject with `reply_subject` set.
4. The subscriber receives the event, processes it, and publishes a reply
   to the reply subject.
5. The one-shot subscription receives the reply and returns it to the caller.
6. If no reply arrives within the timeout, the request returns an error.

This matches NATS request/reply semantics exactly.

---

## Layer 4: EventBus Trait (Public API)

The top-level trait is what external code interacts with. It composes
the storage engine, dispatch engine, delivery channels, and transport
servers behind a single async interface.

```rust
#[async_trait]
pub trait EventBus: Send + Sync + 'static {
    // --- Pub/Sub ---

    /// Publish an event to a subject. The event is persisted durably
    /// before dispatch begins.
    async fn publish(&self, subject: &str, payload: &[u8]) -> Result<u64>;

    /// Subscribe to a subject pattern with the given options.
    /// Returns a Subscription handle for receiving events (in-process)
    /// or a subscription ID (remote delivery).
    async fn subscribe(
        &self,
        pattern: &str,
        opts: SubscribeOpts,
    ) -> Result<Subscription>;

    /// Remove a subscription by ID.
    async fn unsubscribe(&self, id: &str) -> Result<()>;

    // --- Request/Reply ---

    /// Publish a request and wait for a reply. Returns the reply payload
    /// or a timeout error.
    async fn request(
        &self,
        subject: &str,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>>;

    /// Register a request handler on a subject. Incoming requests are
    /// delivered via the subscription; the handler sends replies through
    /// the provided replier.
    async fn serve(
        &self,
        subject: &str,
        opts: SubscribeOpts,
    ) -> Result<RequestSubscription>;

    // --- Delivery Channels ---

    /// Register a custom delivery channel type.
    async fn register_channel_type(
        &self,
        channel_type: &str,
        provider: Arc<dyn DeliveryChannel>,
    ) -> Result<()>;

    // --- Interceptors (convenience for in-process) ---

    /// Register a sync interceptor for a subject pattern.
    /// Shorthand for subscribe with mode=Sync and an InProcess channel.
    async fn intercept(
        &self,
        pattern: &str,
        priority: u32,
        interceptor: Arc<dyn Interceptor>,
    ) -> Result<String>;

    // --- Administration ---

    /// List active subscriptions, optionally filtered by subject pattern.
    async fn list_subscriptions(
        &self,
        filter: Option<&str>,
    ) -> Result<Vec<SubscriptionInfo>>;

    /// Get bus metrics: events processed, pending retries, storage size.
    async fn metrics(&self) -> Result<BusMetrics>;
}

pub struct SubscribeOpts {
    pub priority: u32,
    pub mode: SubscriptionMode,
    pub channel: ChannelConfig,
    pub timeout: Duration,
    pub filter: Option<EventFilter>,
}
```

### Builder Pattern

The bus is constructed via a builder that configures storage, transports,
and operational parameters:

```rust
let bus = EventBusBuilder::new()
    .storage(RedbEventStore::open("./data/eventbus.redb")?)
    .websocket_server("0.0.0.0:9100")
    .http_server("0.0.0.0:9101")
    .compaction_interval(Duration::from_secs(3600))
    .retention(Duration::from_secs(86400))
    .dead_letter_retention(Duration::from_secs(604800))
    .default_interceptor_timeout(Duration::from_secs(5))
    .build()
    .await?;
```

All transport servers are optional. A minimal in-process-only bus:

```rust
let bus = EventBusBuilder::new()
    .storage(RedbEventStore::open("./data/eventbus.redb")?)
    .build()
    .await?;
```

An in-memory bus for testing (no persistence, no transports):

```rust
let bus = EventBusBuilder::new()
    .storage(InMemoryEventStore::new())
    .build()
    .await?;
```

---

## Module Map

```
seidrum-eventbus/
├── src/
│   ├── lib.rs                  # Re-exports, EventBus trait
│   ├── bus.rs                  # EventBusImpl: wires everything together
│   ├── builder.rs              # EventBusBuilder
│   │
│   ├── dispatch/
│   │   ├── mod.rs
│   │   ├── engine.rs           # Dispatch pipeline (persist → chain → fan-out)
│   │   ├── subject_index.rs    # Trie-based subject matching with wildcards
│   │   ├── interceptor.rs      # Interceptor trait, sync chain execution
│   │   └── filter.rs           # EventFilter evaluation
│   │
│   ├── storage/
│   │   ├── mod.rs              # EventStore trait
│   │   ├── redb_store.rs       # redb implementation
│   │   ├── memory_store.rs     # In-memory implementation (testing)
│   │   ├── types.rs            # StoredEvent, EventStatus, DeliveryRecord
│   │   └── compaction.rs       # Background compaction task
│   │
│   ├── delivery/
│   │   ├── mod.rs              # DeliveryChannel trait
│   │   ├── in_process.rs       # tokio::mpsc channel delivery
│   │   ├── websocket.rs        # WebSocket delivery
│   │   ├── webhook.rs          # HTTP POST delivery
│   │   ├── custom.rs           # Plugin-registered channel proxy
│   │   └── retry.rs            # Background retry task
│   │
│   ├── transport/
│   │   ├── mod.rs
│   │   ├── ws_server.rs        # WebSocket server (tokio-tungstenite)
│   │   ├── http_server.rs      # HTTP server (axum)
│   │   └── protocol.rs         # Wire protocol types (JSON message framing)
│   │
│   └── request_reply.rs        # Request/reply implementation
│
├── PROJECT.md
├── ARCHITECTURE.md
├── PLAN.md
├── QUALITY.md
└── Cargo.toml
```

---

## Key Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime, channels, timers |
| `redb` | Embedded ACID storage |
| `axum` | HTTP transport server |
| `tokio-tungstenite` | WebSocket transport server |
| `serde` / `serde_json` | Serialization for protocol and config |
| `tracing` | Structured logging |
| `thiserror` | Error types |

---

## Threading Model

The bus runs entirely within a single tokio runtime. There are no
blocking operations on the dispatch path. The storage engine uses
`spawn_blocking` for redb transactions (redb is not async-native)
but this is isolated behind the `EventStore` trait.

Background tasks:

- **Retry task**: polls `query_retryable()` on a configurable interval
  (default: 5s), re-dispatches failed deliveries.
- **Compaction task**: runs on a longer interval (default: 1h), removes
  old delivered events.
- **Health task**: periodically checks delivery channel health, marks
  unhealthy channels for retry escalation.

All background tasks are spawned as tokio tasks and shut down gracefully
when the bus is dropped.

---

## Error Handling

- Storage errors during write-ahead persist → publish returns Err to caller.
  The event is not dispatched.
- Storage errors during status update → logged, retry on next cycle.
  Not on the critical path.
- Delivery channel errors → DeliveryStatus::Failed, scheduled for retry.
  The event is not lost.
- Sync interceptor timeout → interceptor skipped, warning logged, chain
  continues. The event is not blocked.
- Sync interceptor panic → caught by tokio, treated as timeout. Chain
  continues.

The principle: storage failures before dispatch are hard errors (caller
knows the event was not accepted). Everything after persist is best-effort
with retry. Events are never silently lost.
