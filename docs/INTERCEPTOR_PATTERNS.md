# Interceptor Patterns

How to use sync interceptors to implement cross-cutting concerns that would be complex or impossible with plain pub/sub.

## What interceptors solve

Traditional pub/sub delivers events to subscribers **after** they're published. You can't modify or drop events in transit. This forces every plugin to independently implement:
- Decryption of sensitive payloads
- Rate limiting per user
- Scope/context enrichment
- Audit logging
- Content filtering

With interceptors, these concerns are centralized in the dispatch chain. A single interceptor at the right priority handles it for **all** downstream subscribers.

## The interceptor chain

When an event is published, the bus runs this pipeline:

```
PUBLISH → PERSIST → RESOLVE → FILTER → SYNC CHAIN → ASYNC FAN-OUT → FINALIZE
                                          ↑
                              Interceptors run here
                              (priority order, lowest first)
```

Each interceptor receives `(subject, &mut payload)` and returns:
- **Pass** — continue to the next interceptor unchanged
- **Modified** — continue with the mutated payload
- **Drop** — abort the chain, async subscribers never see the event

## Examples (from Phase 6)

### 1. Encryption interceptor (`examples/encryption_interceptor.rs`)

Two interceptors at priority 100:
- **Inbound**: decrypts `channel.*.inbound` payloads before downstream plugins see plaintext
- **Outbound**: encrypts `channel.*.outbound` payloads before they leave the bus

```
[encrypted payload] → DECRYPT interceptor → [plaintext] → downstream plugins
downstream plugins → [plaintext] → ENCRYPT interceptor → [encrypted payload]
```

No plugin needs to know about encryption — it's transparent.

### 2. Rate limiter (`examples/rate_limiter.rs`)

One interceptor at priority 110 on `channel.*.inbound`:
- Tracks per-user event counts in a sliding window
- **Drops** events that exceed the limit
- Downstream plugins never see rate-limited events

```
alice: msg 1 ✓ → msg 2 ✓ → msg 3 ✓ → msg 4 ✗ (dropped) → msg 5 ✗ (dropped)
```

### 3. Scope tagger (`examples/scope_tagger.rs`)

One interceptor at priority 200 on `channel.*.inbound`:
- Parses the JSON payload
- Injects `"scope": "personal"` (or looks it up from the brain)
- Downstream plugins receive enriched events automatically

```
BEFORE: {"user_id": "alice", "text": "hello"}
AFTER:  {"user_id": "alice", "text": "hello", "scope": "personal"}
```

### 4. Custom delivery channel (`examples/custom_channel.rs`)

Not an interceptor but a related pattern — a plugin registers a custom delivery channel type (`"mqtt"`) so other plugins can route events to external systems:

```rust
bus.register_channel_type("mqtt", Arc::new(MqttChannel)).await?;
```

Subscribers that use `ChannelConfig::Custom { channel_type: "mqtt" }` get their events delivered through the MQTT channel.

## Interceptor vs. subscriber: when to use which

| Need | Use |
|---|---|
| React to events (process, store, forward) | Async subscriber |
| Modify events before other plugins see them | Sync interceptor |
| Drop events before other plugins see them | Sync interceptor |
| Enrich events with metadata | Sync interceptor |
| Route events to external systems | Custom delivery channel |
| Observe events without affecting them | Sync interceptor (return Pass) |

## Priority guidelines

| Range | Use case |
|---|---|
| 1-99 | Kernel-internal (trusted, in-process only) |
| 100-199 | Security (encryption, auth, rate limiting) |
| 200-299 | Enrichment (scope tagging, metadata injection) |
| 300-399 | Transformation (format conversion, filtering) |
| 400+ | Observation (logging, metrics, audit) |

Lower priority = runs first. Remote interceptors (WS/webhook) are clamped to ≥100.

## Running the examples

```bash
# Each example is self-contained with an in-memory bus
cargo run -p seidrum-eventbus --example encryption_interceptor
cargo run -p seidrum-eventbus --example rate_limiter
cargo run -p seidrum-eventbus --example scope_tagger
cargo run -p seidrum-eventbus --example custom_channel
```

## Reference

- [Plugin Author Guide](PLUGIN_AUTHOR_GUIDE.md) — full WS protocol for remote interceptors
- [EventBus Architecture](../crates/core/seidrum-eventbus/ARCHITECTURE.md) — 4-layer design
- [EventBus PLAN.md](../crates/core/seidrum-eventbus/PLAN.md) — Phase 6 deliverables
