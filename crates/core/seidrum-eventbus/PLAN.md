# PLAN.md — seidrum-eventbus Development Plan

## Phasing Strategy

The crate is developed in six phases. Each phase produces a working, tested
artifact that builds on the previous one. No phase requires throwing away
work from an earlier phase. The kernel continues to run on NATS until Phase 5
(migration), so there is no big-bang switchover.

---

## Phase 1: Storage Engine + In-Process Pub/Sub

**Goal:** A working event bus that can publish, subscribe, persist, and
recover events. In-process delivery only. No network, no interceptors,
no wildcards yet.

**Deliverables:**

1. `EventStore` trait and `RedbEventStore` implementation
   - `append()`, `update_status()`, `record_delivery()`
   - `query_by_status()`, `query_by_subject()`
   - Single-table `events` with `subject_idx` and `status_idx`

2. `InMemoryEventStore` for testing (Vec-backed, no disk)

3. Minimal `EventBus` implementation
   - `publish()` → persist → deliver to exact-match subscribers
   - `subscribe()` → InProcess channel only, Async mode only
   - No wildcards, no interceptors, no request/reply

4. `InProcessChannel` delivery
   - `tokio::mpsc` bounded channel per subscription
   - Delivery receipt tracking

5. Basic compaction task
   - Remove delivered events older than retention period

**Tests:** Storage round-trip, publish/subscribe delivery, crash recovery
(write events, drop bus, reopen, verify events are re-dispatched),
compaction correctness.

**Why this first:** The storage engine is the hardest part to get right and
the easiest to test in isolation. Starting here means we validate durability
guarantees before building anything on top.

---

## Phase 2: Subject Wildcards + Interceptor Chains

**Goal:** Full subject matching (wildcards) and the sync/async interceptor
model that is the primary reason this crate exists.

**Deliverables:**

1. `SubjectIndex` trie implementation
   - Exact match, `*` single-token wildcard, `>` terminal wildcard
   - Lookup returns all matching subscriptions sorted by priority

2. Subscription modes: Sync and Async
   - Sync subscribers process sequentially in priority order
   - Async subscribers receive a clone and run in parallel

3. `Interceptor` trait for in-process sync handlers
   - `InterceptResult::Modified`, `Pass`, `Drop`
   - Mutable access to payload bytes

4. Timeout enforcement on sync interceptors
   - Configurable per-subscription, default from builder
   - Timed-out interceptors are skipped with a warning

5. `EventFilter` evaluation
   - `FieldEquals`, `FieldContains`, `All`, `Any`
   - Applied after subject match, before delivery

6. Full dispatch pipeline
   - Persist → resolve → filter → sync chain → async fan-out → finalize

**Tests:** Wildcard matching (comprehensive pattern matrix), interceptor
ordering, interceptor mutation propagation, interceptor drop aborts chain,
interceptor timeout does not stall, event filter correctness, mixed
sync/async delivery ordering.

**Why this second:** Interceptors are the core differentiator. With Phase 1
providing a solid storage foundation, we can build the dispatch engine with
confidence that events will not be lost even if the chain logic has bugs
during development.

---

## Phase 3: Request/Reply

**Goal:** Support the request/reply pattern that Seidrum uses for brain
queries, capability calls, and health checks.

**Deliverables:**

1. `request()` implementation
   - Generate unique reply subject
   - Create one-shot subscription on reply subject
   - Publish event with `reply_subject` field
   - Wait for reply with timeout
   - Clean up one-shot subscription on completion or timeout

2. `serve()` implementation
   - Subscribe to a subject like normal
   - Received events include a `Replier` handle
   - Handler calls `replier.reply(payload)` to send the response
   - Replier publishes to the event's `reply_subject`

3. Request timeout handling
   - Configurable per-request
   - Timeout returns `Err(RequestTimeout)` to caller
   - One-shot subscription is cleaned up

**Tests:** Basic request/reply round-trip, timeout behavior, multiple
concurrent requests on the same subject, request to subject with no
subscribers (timeout), request/reply through interceptor chain (interceptor
can modify the request before the handler sees it).

**Why this third:** Request/reply is built on pub/sub, so it naturally
follows Phase 2. Many Seidrum interactions depend on this pattern, so it
must work before migration.

---

## Phase 4: Transport Servers + Remote Delivery Channels

**Goal:** Network-accessible bus. Remote plugins can connect via WebSocket
or HTTP.

**Deliverables:**

1. WebSocket transport server
   - `tokio-tungstenite` listener on configurable address
   - Authentication handshake (API key or JWT)
   - JSON-framed protocol: publish, subscribe, request, reply, register
   - Connection tracking: assign connection_id, map to subscriptions
   - Disconnect handling: clean up subscriptions, fail pending deliveries

2. HTTP transport server
   - `axum` on configurable address
   - `POST /publish`, `POST /request`, `POST /subscribe`,
     `DELETE /subscribe/{id}`, `GET /events/{seq}`, `GET /health`
   - Webhook subscription persistence (survives restart)

3. `WebSocketChannel` delivery
   - Deliver events to a specific WebSocket connection
   - Handle connection drops gracefully (fail → retry queue)

4. `WebhookChannel` delivery
   - HTTP POST per event with configurable headers
   - Retry policy: configurable max_attempts, backoff, max_backoff
   - HTTP status 2xx = delivered, anything else = failed

5. Background retry task
   - Poll `query_retryable()` periodically
   - Re-attempt delivery through the original channel
   - Exponential backoff with jitter
   - Move to DeadLettered after max_attempts

6. Sync interceptor protocol for remote subscribers
   - WebSocket: send event, wait for Modified/Pass/Drop response
   - Webhook: POST event, response body contains Modified/Pass/Drop
   - Timeout enforcement applies identically to in-process

7. Custom delivery channel registry
   - `register_channel_type()` stores `Arc<dyn DeliveryChannel>`
   - Subscriptions with `ChannelConfig::Custom` delegate to registered
     provider
   - Unregistered channel type → subscription error

**Tests:** WebSocket connect/subscribe/publish round-trip, WebSocket
disconnect and reconnect with message replay, webhook delivery and retry,
webhook with auth headers, remote sync interceptor via WebSocket, custom
channel registration and delivery, HTTP API endpoint tests.

**Why this fourth:** Transport is additive. The bus already works in-process.
Adding network access does not change the core dispatch logic — it adds new
delivery channel implementations and protocol handling.

---

## Phase 5: Seidrum Integration + NATS Migration

**Goal:** Replace NATS in the Seidrum kernel with `seidrum-eventbus`.
All existing functionality continues to work.

**Deliverables:**

1. Adapter layer in `seidrum-common`
   - Replace `BusClient` internals with `EventBus` calls
   - `publish_envelope()` → serialize EventEnvelope → `bus.publish()`
   - `request()` → `bus.request()`
   - `subscribe()` → `bus.subscribe()` with Async mode

2. Kernel startup changes
   - Create `EventBusBuilder` instead of NATS connection
   - Start WebSocket + HTTP servers for remote plugins
   - Kernel services register as in-process subscribers

3. Plugin bootstrap migration
   - Rust plugins: `BusClient` already wraps the bus — no source change
   - API gateway: adapt WebSocket protocol to new bus protocol
   - External plugins: WebSocket protocol is similar, adapt framing

4. Subject mapping verification
   - All existing NATS subjects work unchanged
   - Wildcard patterns work unchanged
   - Request/reply subjects work unchanged

5. Remove NATS dependency
   - Remove `async-nats` from Cargo.toml
   - Remove NATS URL from platform.yaml
   - Remove NATS from docker-compose.yml
   - Update documentation

**Tests:** Full e2e test suite passes against the new bus. Compare event
flow traces before and after migration to verify identical behavior.

**Why this fifth:** The bus is fully functional and independently tested
before we touch the kernel. Migration is a plumbing exercise, not a
design exercise. If something breaks, we know the bus works — the bug
is in the integration layer.

---

## Phase 6: Interceptor-Based Plugin Patterns

**Goal:** Demonstrate the new interceptor model with concrete plugin
rewrites that would have been complex or impossible with NATS.

**Deliverables:**

1. Example: Encryption interceptor
   - Sync interceptor at priority 5 on `channel.*.inbound`
   - Decrypts message payload before downstream plugins see it
   - Corresponding interceptor on `channel.*.outbound` encrypts

2. Example: Rate limiter interceptor
   - Sync interceptor at priority 10 on `channel.*.inbound`
   - Drops events from users exceeding rate limits
   - No subject rewiring needed

3. Example: Scope tagger interceptor
   - Sync interceptor at priority 15 on `channel.*.inbound`
   - Annotates events with user scope context
   - Downstream plugins receive enriched events automatically

4. Example: Multi-channel delivery plugin
   - Plugin registers an `"mqtt"` custom delivery channel
   - Other plugins can request MQTT delivery for IoT events

5. Documentation: Plugin author guide
   - How to write a sync interceptor
   - How to choose priority values
   - How to register a custom delivery channel
   - Migration guide from NATS-based plugins

**Tests:** Each example plugin has its own integration tests. End-to-end
test verifying a multi-interceptor chain with mixed sync/async subscribers.

**Why this last:** These are the patterns that justify the entire project.
By this point the bus is stable and integrated. These examples prove the
design and serve as templates for future plugin development.

---

## Milestone Summary

| Phase | Deliverable | Depends On | Estimated Complexity |
|-------|------------|------------|---------------------|
| 1 | Storage + in-process pub/sub | Nothing | Medium |
| 2 | Wildcards + interceptor chains | Phase 1 | High |
| 3 | Request/reply | Phase 2 | Low-Medium |
| 4 | Transports + remote delivery | Phase 3 | High |
| 5 | Kernel migration | Phase 4 | Medium |
| 6 | Interceptor patterns | Phase 5 | Low |

---

## Risk Register

**redb performance under write-ahead load.** Risk: redb write transactions
are slower than expected, causing publish latency to spike. Mitigation:
benchmark in Phase 1 before building further. Fallback: switch to a custom
append-only log file with manual indexing, or use `sled` / `sqlite` WAL mode.

**Sync interceptor deadlocks.** Risk: a sync interceptor publishes an event
that triggers another sync interceptor, creating a circular dependency.
Mitigation: detect re-entrant publish calls and fail with an error rather
than deadlocking. Document that sync interceptors must not publish to
subjects they intercept.

**WebSocket protocol complexity.** Risk: the remote interceptor protocol
(sync request/reply over WebSocket) is harder to implement correctly than
anticipated. Mitigation: start with async-only remote subscribers in Phase 4
and add sync remote interceptors as a sub-phase.

**Migration breaks existing e2e tests.** Risk: subtle behavioral differences
between NATS and the new bus cause test failures. Mitigation: run the full
e2e suite against both backends in parallel during Phase 5 and diff the
event traces.

**Custom channel provider disconnects mid-delivery.** Risk: a plugin that
registered a custom channel type crashes while the bus is trying to deliver
through it. Mitigation: delivery failure → retry queue, same as any other
channel failure. If the provider never reconnects, events dead-letter after
max retries.
