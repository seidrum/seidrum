# PROJECT.md — seidrum-eventbus

## What This Is

`seidrum-eventbus` is a standalone Rust crate that replaces NATS JetStream as
Seidrum's event backbone. It is a purpose-built event bus with three
capabilities that NATS does not provide together: interceptor chains with
priority-ordered mutation, configurable per-subscription delivery channels,
and durable event storage with indexed retry.

The crate has zero dependency on the rest of the Seidrum kernel. It can be
embedded in any Rust binary or exposed over the network via its built-in
HTTP and WebSocket transports.

## Why We Are Building This

### NATS is more than we need — and less

NATS is a general-purpose message broker. Seidrum uses a narrow slice of it:
pub/sub with subject routing, request/reply, and JetStream for durability.
We pay the operational cost of running and configuring an external NATS server,
but we do not use clustering, leaf nodes, accounts, or most of JetStream's
consumer model. The dependency is heavy for what we get.

At the same time, NATS lacks something fundamental to our plugin model:
the ability for a subscriber to intercept and mutate an event before
downstream subscribers see it. Today, achieving this requires a two-subject
hop pattern where each intercepting plugin consumes from one subject and
produces to another, baking pipeline order into subject naming. This makes
plugins aware of each other and makes the topology brittle.

### What we gain by owning the event layer

**Interceptor chains.** Subscriptions declare a numeric priority and a
sync/async mode. Sync interceptors receive a mutable reference to the event
and process sequentially in priority order. Each handler can mutate fields,
enrich metadata, or drop the event entirely. Async subscribers receive a
clone after the sync chain completes and run in parallel. This turns
"intercept X and resend it" into a one-line subscription declaration instead
of a multi-subject rewiring exercise.

**Per-subscription delivery channels.** When a plugin subscribes to an event
subject, it declares how it wants events delivered: in-process tokio channel,
persistent WebSocket, HTTP webhook, or a custom channel type registered by
another plugin. A single plugin can use different delivery channels for
different subscriptions. This means a Rust plugin compiled into the kernel
gets zero-serialization in-process delivery for hot-path events, while a
Python plugin gets WebSocket delivery, and a serverless function gets
webhook delivery — all subscribing to the same subject.

**Plugin-provided delivery channels.** Plugins can register new channel
types with the bus. An MQTT plugin could register an `"mqtt"` channel type,
and any other plugin could then request MQTT delivery for specific
subscriptions. The bus delegates delivery to the channel provider plugin
without knowing anything about the transport protocol.

**Durable event storage with indexed retry.** Events are persisted to an
embedded database before dispatch. Each event has a lifecycle status (Pending,
Dispatching, Delivered, PartiallyDelivered, DeadLettered) and per-subscriber
delivery tracking. On crash recovery, the bus scans for incomplete events
and re-enters them into the pipeline. Failed deliveries are retried with
configurable backoff. The storage is indexed by subject and status for
efficient queries.

**No external infrastructure.** The embedded storage engine (redb or
equivalent) runs in-process. There is no server to start, no port to
configure, no Docker container to manage. The bus starts when the kernel
starts.

**Faster tests.** Unit and integration tests can spin up an in-process event
bus with no Docker, no network, no port conflicts. Test isolation becomes
trivial.

### What we give up

**Battle-tested clustering.** NATS has mature clustering, super-clusters, and
leaf nodes. We are building for a single-node deployment (personal AI agent).
If multi-node becomes a requirement, the `EventBus` trait can be implemented
by a networked backend, but that is explicitly out of scope for now.

**Ecosystem compatibility.** NATS clients exist for every language. Our
WebSocket and HTTP transports cover the same use case (any-language plugins),
but plugins that already use a NATS client library will need to migrate to
our protocol. The WebSocket protocol is simpler than NATS, so migration
effort is low.

**Community support.** NATS has a large community and extensive documentation.
We own all bugs and all maintenance. The tradeoff is acceptable because the
surface area is bounded: we are building exactly the features Seidrum needs,
not a general-purpose broker.

## Design Constraints

1. **The crate must have no dependency on seidrum-kernel or seidrum-common.**
   It is a leaf dependency, not a framework. Seidrum types (EventEnvelope,
   plugin structs) live in seidrum-common and use the bus through its public
   API.

2. **The interceptor chain must not block on persistence.** Events are
   persisted before dispatch (write-ahead), but the persist step must be
   fast enough (single append, microseconds) that it does not meaningfully
   delay the sync chain.

3. **Sync interceptors must have timeouts.** A misbehaving handler cannot
   stall the entire pipeline. Timed-out handlers are skipped, and the event
   continues down the chain. Timeout violations are logged and surfaced as
   metrics.

4. **The bus must support subject wildcards.** Patterns like `channel.*.inbound`
   and `brain.>` must work for both publishing and subscribing, matching
   NATS wildcard semantics that Seidrum already relies on.

5. **Request/reply must be a first-class pattern.** Many Seidrum interactions
   (brain queries, capability calls, health checks) use request/reply. The
   bus must support this natively, not as a pub/sub workaround.

6. **Delivery channel failures must not lose events.** If a WebSocket
   disconnects or a webhook returns 500, the event stays in
   PartiallyDelivered status and is retried. Only after exhausting retries
   does it move to DeadLettered.

7. **The public API must be async-first.** All operations return futures.
   The bus is designed for tokio and does not support synchronous callers.

## Crate Name and Location

- Crate: `seidrum-eventbus`
- Path: `crates/core/seidrum-eventbus/`
- Workspace member alongside `seidrum-common`, `seidrum-kernel`, etc.
- Published API: the `EventBus` trait, transport servers, storage types
