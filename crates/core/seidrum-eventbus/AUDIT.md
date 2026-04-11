# AUDIT.md — Current State vs. Spec

This document compares the current state of `seidrum-eventbus` (commit
`94ed20c` on main, the merged Phase 5 PR #27) against the canonical
specifications restored on this branch:

- [`PROJECT.md`](./PROJECT.md) — vision and design constraints
- [`ARCHITECTURE.md`](./ARCHITECTURE.md) — internal architecture, trait definitions, module map
- [`PLAN.md`](./PLAN.md) — phased development plan
- [`QUALITY.md`](./QUALITY.md) — testing and quality strategy

The audit is grouped by **layer** and within each layer by **conformance status**.
Where the current code diverges from the spec, the audit notes whether the
divergence is:

- **OK** — intentional or improved upon spec, no action needed
- **GAP** — spec'd functionality is missing
- **DRIFT** — current code does something different than spec; needs reconciliation
- **EXTRA** — current code has functionality not described in spec

---

## Overall Phase Status

| Phase (per PLAN.md) | Status | Notes |
|---|---|---|
| Phase 1: Storage + in-process pub/sub | ✅ Done | Shipped as PR #20 |
| Phase 2: Wildcards + interceptor chains | ✅ Done | Shipped as PR #21 |
| Phase 3: Request/reply | ✅ Done | Shipped as PR #22 |
| Phase 4: Transports + retry/dead-letter + custom channels + remote sync interceptor protocol | ⚠️ **Partial** | PR #23 + PR #27 finished most of Phase 4. **Remote sync interceptor protocol over WebSocket/webhook is missing.** |
| Phase 5: Seidrum kernel migration / NATS replacement | ❌ Not started | What we labeled "Phase 5" was actually completing Phase 4. Real Phase 5 is the kernel migration described in PLAN.md lines 137-179. |
| Phase 6: Interceptor-based plugin patterns + example plugins | ❌ Not started | |

---

## Layer 1 — Storage Engine

### EventStore trait

| Spec method (ARCHITECTURE.md) | Current state | Status |
|---|---|---|
| `append(&StoredEvent) -> Result<u64>` | Present | ✅ OK |
| `update_status(seq, status)` | Present | ✅ OK |
| `record_delivery(seq, sub_id, status)` | Present, **with extra params**: `error: Option<String>`, `next_retry: Option<u64>` | EXTRA — needed for proper retry plumbing. Already documented; spec should be updated to match. |
| `query_by_status(status, limit)` | Present | ✅ OK |
| `query_by_subject(pattern, since, limit)` | Present, but supports **exact match only**, not pattern | DRIFT — the trait method is named "by_subject" with a `pattern` param in the spec, but the current impl is exact-only. Spec ARCHITECTURE.md L65-70 says "by subject pattern". Either rename or implement pattern matching. |
| `query_retryable(max_attempts, limit)` | Present, ordered by `next_retry` | ✅ OK |
| `compact(older_than)` | Present | ✅ OK |
| `get(seq)` | Present | EXTRA — added during Phase 5 review sweep for `maybe_finalize_event`. Useful, not in spec. |
| `count_retryable(max_attempts)` | Present | EXTRA — added for the metrics endpoint. Not in spec. |
| `query_dead_lettered(limit)` | Present (default impl) | EXTRA — convenience helper. |

### StoredEvent / DeliveryRecord schema

| Spec field (ARCHITECTURE.md L88-132) | Current state | Status |
|---|---|---|
| `StoredEvent { seq, subject, payload, stored_at, status, deliveries, reply_subject }` | All present | ✅ OK |
| `EventStatus { Pending, Dispatching, Delivered, PartiallyDelivered, DeadLettered }` | All present | ✅ OK |
| `DeliveryRecord { subscriber_id, status, attempts, last_attempt, next_retry, error }` | All present | ✅ OK |
| `DeliveryStatus { Pending, Delivered, Failed, DeadLettered }` | All present | ✅ OK |
| `DeliveryRecord::is_retryable(max, now)` helper | Present | EXTRA — added during review sweep. Useful. |

### redb table layout

ARCHITECTURE.md L140-145 specifies four tables:

| Spec table | Key | Value | Current state |
|---|---|---|---|
| `events` | `seq: u64` | `StoredEvent` | ✅ Present |
| `subject_idx` | `(subject, seq)` | `()` | ✅ Present |
| `status_idx` | `(status, seq)` | `()` | ✅ Present |
| `delivery_idx` | `(subscriber_id, seq)` | `DeliveryStatus` | **GAP — missing**. Current code has `failed_deliveries_idx` keyed by `seq` (no subscriber_id). The spec table would enable per-subscriber retry queries; the current table only enables "events with at least one failure". This is a meaningful divergence but defensible — see notes. |

### Compaction

| Spec | Current | Status |
|---|---|---|
| Removes Delivered events older than retention (default 24h) | Yes | ✅ OK |
| Removes DeadLettered events older than dead-letter retention (default 7d) | **No** — DeadLettered events use the same retention as Delivered | DRIFT — spec ARCHITECTURE.md L156-159 wants longer dead-letter retention "for debugging". Current `compact()` treats both terminal states identically. |
| Background task with configurable interval (default 1h) | Yes | ✅ OK |
| Graceful shutdown via watch signal | Yes | EXTRA — improved upon spec (not explicitly required) |

### Storage tests (per QUALITY.md L60-74)

| Required test | Present? |
|---|---|
| `test_append_and_query` | ✅ |
| `test_seq_monotonic` | ✅ |
| `test_status_update` | ✅ |
| `test_delivery_recording` | ✅ |
| `test_query_by_status_filter` | ✅ |
| `test_query_by_subject_exact` | ✅ |
| `test_query_by_subject_pattern` | **GAP — missing** (because pattern query is not implemented) |
| `test_query_retryable` | ✅ (covered indirectly via Phase 5 retry tests) |
| `test_compaction_delivered` | ✅ |
| `test_compaction_preserves_deadletter` | **GAP** — needed once dead-letter retention is implemented |
| `test_crash_recovery` | ✅ (`test_redb_crash_recovery`, plus `test_redb_failed_delivery_survives_restart`) |
| `test_concurrent_appends` | ✅ (`test_memory_store_concurrent_appends`) — but only in-memory, not against ReDB |
| `bench_append_throughput` | **GAP** — no benchmarks |

---

## Layer 2 — Delivery Channels and Transport Servers

### DeliveryChannel trait

| Spec | Current | Status |
|---|---|---|
| `deliver(event, subject, config) -> Result<DeliveryReceipt>` | Present | ✅ OK |
| `cleanup(config)` | Present | ✅ OK |
| `is_healthy(config)` | Present | ✅ OK |
| `DeliveryReceipt { delivered_at, latency_us }` | Present | ✅ OK |
| `DeliveryError` enum | Present, with `Failed`, `Permanent`, `NotReady` | EXTRA — `Permanent` was added during Phase 5 review for HTTP 4xx classification. Not in spec but improvement. |

### Built-in delivery channels

| Spec channel | Present? | Notes |
|---|---|---|
| `InProcessChannel` (tokio::mpsc) | ✅ Standalone implementation in `delivery/in_process.rs` | The dispatch engine **does not actually use this** for in-process subscribers — it uses raw mpsc senders stored in `DispatchState::senders`. The standalone `InProcessChannel` exists as an implementation of the `DeliveryChannel` trait but is dead code from a user perspective. |
| `WebSocketChannel` | ✅ `delivery/websocket.rs` | Standalone — same caveat as above. The transport WS server uses its own forwarder, not this channel. |
| `WebhookChannel` | ✅ `delivery/webhook.rs` | This one **is** used: by both the HTTP transport's subscription handler and the dispatch engine's retry path. |
| `CustomChannel` proxy | **GAP — no proxy struct** | The `ChannelRegistry` exists and the engine looks up `Custom { channel_type }` channels at retry time, but there's no `CustomChannel` struct. The spec describes it as "A proxy that delegates to a plugin-registered impl" — currently the engine inlines this lookup. Not strictly missing functionality, just missing the named type. |

### ChannelConfig

ARCHITECTURE.md L226-241:

| Spec variant | Current | Status |
|---|---|---|
| `InProcess` | ✅ | OK |
| `WebSocket { connection_id }` | Current is unit variant `WebSocket` (no connection_id) | DRIFT — connection_id was removed during review sweep as "dead metadata". Spec includes it. The spec usage was vague, so the divergence is defensible. |
| `Webhook { url, headers, retry: RetryPolicy }` | Current is `Webhook { url, headers }` (no embedded RetryPolicy) | DRIFT — current code has retry config at the bus/builder level (`RetryConfig`), not per-subscription. Spec wanted per-subscription retry policy. Bus-wide is simpler and matches the actual use case but is a real divergence. |
| `Custom { channel_type, config }` | ✅ | OK |
| `RetryPolicy { max_attempts, initial_backoff, multiplier, max_backoff }` | Current `RetryConfig { max_attempts, initial_backoff_ms, max_backoff_ms }` (no `multiplier`) | DRIFT — spec has separate multiplier; current code hardcodes 2x exponential. Same field semantics otherwise. |

### Transport servers

| Spec endpoint / capability | Current state | Status |
|---|---|---|
| WS server: `tokio-tungstenite`, configurable address | ✅ | OK |
| WS server: API key / JWT auth | ✅ `Authenticator` trait + `AuthRequest` with peer_addr + headers | OK |
| WS subscribe / publish / request | ✅ JSON protocol | OK |
| WS register custom delivery channel type | **GAP** — no protocol message for this | Spec wants plugins to be able to register channels over WS. Today the registry is only writable from in-process Rust code. |
| **WS sync interceptor protocol** (request/reply for Modified/Pass/Drop) | **GAP — entirely missing** | This is the biggest Phase 4 gap. Remote subscribers can subscribe `Async` only — they cannot participate in the sync interceptor chain. ARCHITECTURE.md L436-440 and PLAN.md L154 require this. |
| HTTP `POST /publish` | ✅ | OK |
| HTTP `POST /request` | ✅ | OK |
| HTTP `POST /subscribe` (webhook subscription) | ✅ | OK |
| HTTP `DELETE /subscribe/:id` | ✅ | OK |
| HTTP `GET /events/:seq` | **GAP — removed during review sweep** as a stub. Spec lists it as a deliverable. |
| HTTP `GET /health` | ✅ | OK |
| HTTP `GET /metrics` | ✅ | OK |
| Webhook subscriptions persisted (survive restart) | **GAP** — current webhook subscriptions live in dispatch state only; on restart they're gone. |
| **Webhook sync interceptor protocol** (response body indicates Modified/Pass/Drop) | **GAP** — same as WS interceptor protocol gap |

### Delivery channel tests (QUALITY.md L137-149)

| Required test | Present? |
|---|---|
| `test_in_process_roundtrip` | ✅ |
| `test_in_process_backpressure` | **GAP** |
| `test_ws_delivery` | **GAP** — no real WS server end-to-end test |
| `test_ws_disconnect_failure` | **GAP** |
| `test_ws_reconnect_resume` | **GAP** |
| `test_webhook_success` | **GAP** — no real HTTP mock server test |
| `test_webhook_server_error` | **GAP** |
| `test_webhook_connection_refused` | **GAP** |
| `test_webhook_custom_headers` | **GAP** |
| `test_custom_channel_delegation` | ✅ (Phase 5 retry tests cover this indirectly) |
| `test_custom_unregistered_error` | ✅ (`test_retry_permanent_error_dead_letters_immediately`) |

---

## Layer 3 — Dispatch Engine

### Subject index

| Spec | Current | Status |
|---|---|---|
| Trie keyed by subject tokens | ✅ | OK |
| `*` single-token wildcard | ✅ | OK |
| `>` terminal wildcard | ✅ | OK |
| Lookup O(depth) | ✅ | OK |
| `SubjectIndex::find_by_id` | Present | EXTRA — needed for retry path. |

### Event filters (QUALITY.md L113-122)

| Required test | Present? |
|---|---|
| `test_field_equals_match` | ✅ |
| `test_field_equals_reject` | ✅ |
| `test_field_contains` | ✅ |
| `test_nested_path` | ✅ |
| `test_all_combinator` | ✅ |
| `test_any_combinator` | ✅ |
| `test_missing_field` | ✅ (covered by `test_field_equals_missing_field`) |

### Dispatch pipeline (ARCHITECTURE.md L370-406)

The 6-stage pipeline (PERSIST → RESOLVE → FILTER → SYNC → ASYNC FAN-OUT → FINALIZE) is fully implemented.

**Notable divergences from spec:**

| Spec behavior | Current behavior | Status |
|---|---|---|
| Async fan-out: "Spawn a delivery task for each async subscriber concurrently" | Current implementation **iterates async senders sequentially** with `try_send`. The pipeline doc on `DispatchEngine` says so explicitly. | DRIFT — spec wants `tokio::spawn` per subscriber for true concurrency. Current is non-blocking sequential. Same result for fast subscribers; slow subscribers serialize. |
| "Spawn a delivery task for each async subscriber" → "On failure → record DeliveryStatus::Failed, schedule retry" | Current records via `try_record` helper with computed `next_retry` | ✅ OK |
| Sync chain holds `RwLock` only during snapshot | ✅ Snapshot pattern is in place | OK (after Phase 5 review fix) |
| Re-entrant publish from sync interceptor returns error | **GAP — no detection** | The constraint is documented in PROJECT.md L101-103 and QUALITY.md L108 (`test_reentrant_error`). Currently a sync interceptor that calls `bus.publish` on a subject it intercepts will deadlock or recurse forever. |

### Interceptor tests (QUALITY.md L93-109)

| Required test | Present? |
|---|---|
| `test_priority_order` | ✅ (`test_interceptor_chain_ordering`) |
| `test_mutation` | ✅ (`test_interceptor_modifies_payload`) |
| `test_mutation_propagation` | ✅ |
| `test_mutation_visible_to_async` | ✅ |
| `test_drop_aborts` | ✅ |
| `test_drop_prevents_async` | ✅ (`test_interceptor_drops_event`) |
| `test_pass_unchanged` | ✅ |
| `test_timeout_skip` | ✅ (`test_interceptor_timeout_skips`) |
| `test_timeout_no_stall` | ✅ (covered by timeout test) |
| `test_panic_recovery` | **GAP** — no test for a panicking interceptor |
| `test_async_concurrent` | **GAP** — but see async fan-out divergence above |
| `test_mixed_ordering` | ✅ (`test_mixed_sync_async_delivery`) |
| `test_reentrant_error` | **GAP** — no detection, no test |
| `test_no_sync_passthrough` | ✅ (`test_subscribe_and_receive` covers this trivially) |

### Request/reply tests (QUALITY.md L125-133)

| Required test | Present? |
|---|---|
| `test_basic_roundtrip` | ✅ |
| `test_timeout_no_handler` | ✅ |
| `test_timeout_slow_handler` | ✅ |
| `test_concurrent_requests` | ✅ |
| `test_through_interceptor` | ✅ |
| `test_cleanup_on_complete` | ✅ |
| `test_cleanup_on_timeout` | ✅ |

---

## Layer 4 — EventBus Trait (Public API)

| Spec method (ARCHITECTURE.md L463-534) | Current | Status |
|---|---|---|
| `publish(subject, payload) -> Result<u64>` | ✅ | OK |
| `subscribe(pattern, opts) -> Result<Subscription>` | ✅ | OK |
| `unsubscribe(id)` | ✅ | OK |
| `request(subject, payload, timeout) -> Result<Vec<u8>>` | ✅ | OK |
| `serve(subject, opts) -> Result<RequestSubscription>` | ✅ But signature differs: takes individual params (`priority`, `timeout`, `filter`) instead of `SubscribeOpts`. Justified by the previous review. | DRIFT (intentional) |
| `register_channel_type(channel_type, provider)` | **GAP — not on EventBus trait**. The `ChannelRegistry` has `register()` but it's only accessible via the builder's `register_channel()` method, not at runtime via the bus. Spec wants this on the trait so plugins can register channels at runtime over the network. |
| `intercept(pattern, priority, interceptor)` (shorthand) | ✅ With extra `timeout` param | OK |
| `list_subscriptions(filter)` | ✅ Filter is exact-match per the review | OK |
| `metrics()` | ✅ | OK |

### `BusMetrics` shape

| Spec | Current | Status |
|---|---|---|
| events processed | `events_published` ✅ | OK |
| events delivered | `events_delivered` ✅ | OK |
| pending retries | `events_pending_retry` ✅ (gated on `retry_enabled`) | OK |
| storage size | **GAP** — not exposed | Spec ARCHITECTURE.md L533 mentions "storage size". |
| subscription count | ✅ | OK |

### Builder

| Spec builder method | Current | Status |
|---|---|---|
| `storage(impl)` | ✅ | OK |
| `websocket_server(addr)` | ✅ as `with_websocket(addr)` | OK |
| `http_server(addr)` | ✅ as `with_http(addr)` | OK |
| `compaction_interval(d)` | ✅ | OK |
| `retention(d)` | ✅ | OK |
| `dead_letter_retention(d)` | **GAP** — not present | Tied to the "longer dead-letter retention" gap above. |
| `default_interceptor_timeout(d)` | **GAP** — interceptor timeout is set per-subscription only (`SubscribeOpts::timeout`); no bus-wide default. |
| `with_retry`, `with_retry_poll_interval`, `with_retry_query_limit` | ✅ | EXTRA — not in spec but needed |
| `with_channel_registry`, `register_channel` | ✅ | EXTRA — needed for shared registry plumbing |
| `build()` | ✅ | OK |

### Test utilities (QUALITY.md L196-239)

| Spec utility | Current | Status |
|---|---|---|
| `test_utils::test_bus()` | ✅ | OK |
| `test_utils::test_bus_with_storage()` | **GAP** |
| `test_utils::test_bus_with_transports()` | **GAP** |
| `publish_and_wait(...)` | **GAP** |
| `collect_events(...)` | **GAP** |
| `RecordingInterceptor` | **GAP** |
| `MockWebhookServer` | **GAP** |

---

## Threading and Background Tasks

| Spec (ARCHITECTURE.md L646-662) | Current | Status |
|---|---|---|
| Single tokio runtime, no blocking on dispatch | ✅ | OK |
| Storage `spawn_blocking` for redb | ✅ | OK |
| Retry task: poll `query_retryable()` interval (default 5s) | ✅ Default is **1s** in builder | DRIFT (minor) |
| Compaction task: longer interval (default 1h) | ✅ | OK |
| Health task: check delivery channel health | **GAP** — no periodic health task |
| Graceful shutdown via tokio task drop | ✅ Via watch channel + `shutdown_and_join` | OK |

---

## Cross-cutting Gaps

### Tracing and observability (QUALITY.md L324-338)

| Spec | Current |
|---|---|
| `tracing` spans on bus operations | Mostly inline `tracing::warn!` / `debug!` calls; no proper `#[tracing::instrument]` spans | DRIFT |
| `INFO` for bus started, transport listening, sub added/removed | Partial — bus started yes, sub add/remove no | DRIFT |
| `DEBUG` for event published, delivery started/completed | Yes for published; partial for delivery | OK-ish |
| `WARN` for interceptor timeout, delivery failure, channel unhealthy | ✅ | OK |
| `ERROR` for storage write failed, max retries exhausted, transport error | ✅ | OK |

### Benchmarks (QUALITY.md L257-269)

**GAP — no benchmark suite exists.** Spec lists 6 benchmarks with concrete targets (`bench_append`, `bench_publish_deliver`, `bench_subject_lookup`, `bench_interceptor_chain`, `bench_ws_roundtrip`, `bench_concurrent_publish`). None exist in `benches/`.

### Property-based testing (QUALITY.md L243-254)

**GAP — no `proptest` dependency, no property tests.** Spec recommends three areas (subject index, event filters, interceptor ordering).

### Chaos / fault injection (QUALITY.md L274-291)

**GAP — no chaos test suite.** Spec lists 5 fault injection scenarios, none implemented.

### CI policy (QUALITY.md L307-321)

The current CI runs `cargo test`, `cargo clippy`, `cargo fmt`. The spec also requires:
- `cargo doc -p seidrum-eventbus --no-deps` on every PR — **GAP** (we run it locally but it's not in CI)
- `cargo test -- --ignored` on merge to main — **GAP** (no ignored tests exist either)
- `cargo bench` on merge to main — **GAP** (no benchmarks)

---

## Summary of Top Adjustments

Sorted by impact:

### Phase 4 completion (must finish before real Phase 5)

1. **Remote sync interceptor protocol over WebSocket and webhook.** Spec ARCHITECTURE.md L436-440 + PLAN.md L154. Currently remote subscribers are async-only. This is the headline missing Phase 4 deliverable.
2. **`register_channel_type` exposed on the `EventBus` trait** so remote plugins (or in-process plugins added at runtime) can register custom channel types over the network. ARCHITECTURE.md L506-512.
3. **Webhook subscription persistence** so webhook subs survive restart. PLAN.md L132 explicitly calls this out.
4. **`HttpServer` `GET /events/:seq` endpoint** restored — was removed during review sweep but is in the spec.

### Storage spec drift

5. **`delivery_idx` table keyed by `(subscriber_id, seq) → DeliveryStatus`** per ARCHITECTURE.md L145. Current `failed_deliveries_idx` keyed by `seq` is a simpler version. Decide: align with spec, or update the spec to match the simpler current design.
6. **Dead-letter retention** as a separate setting from the regular retention. Default 7 days per spec.
7. **`query_by_subject` with pattern matching** (or rename to `_exact` to make the limitation explicit).
8. **Storage size in `BusMetrics`** (or remove from spec).

### Test gaps

9. **`test_panic_recovery`** — verify a panicking sync interceptor is caught and the chain continues.
10. **`test_reentrant_error`** — but first **detect** re-entrant publish from a sync interceptor. Today it would deadlock or recurse.
11. **End-to-end transport tests** — `test_ws_subscribe_receive`, `test_ws_remote_publish`, `test_http_publish`, etc. The current `phase4_transport.rs` tests don't actually start a server and connect to it.
12. **Webhook integration tests** with a `MockWebhookServer` test utility.
13. **`test_concurrent_appends` for ReDB** (currently only in-memory).

### Test utilities

14. **`test_utils::test_bus_with_storage`, `test_bus_with_transports`, `publish_and_wait`, `collect_events`, `RecordingInterceptor`, `MockWebhookServer`** — none of the QUALITY.md-mandated test utilities exist beyond the basic `test_bus()`.

### Observability

15. **Proper `#[tracing::instrument]` spans** on `publish_event`, `retry_to_subscriber`, `dispatch` stages.
16. **Structured `INFO` logs** on subscription add/remove and bus startup.

### Non-goals for now

These are spec'd but lower priority and arguably can wait until after the kernel migration (real Phase 5):

- Benchmarks (`benches/`) — useful but not blocking
- Property-based testing — useful but not blocking
- Chaos / fault injection — useful but not blocking
- Health task for delivery channels
- `default_interceptor_timeout` builder method

---

## Recommended Next Move

Given the gap analysis, the cleanest path forward:

1. **Finish Phase 4 properly** on a new branch:
   - Remote sync interceptor protocol (WS + webhook)
   - `register_channel_type` on the `EventBus` trait + WS protocol message
   - Webhook subscription persistence
   - Restore `GET /events/:seq`
   - Fill the test utility gaps (`MockWebhookServer`, `RecordingInterceptor`, etc.)
   - Add the missing transport integration tests

2. **Then start real Phase 5** (kernel migration) on its own branch, replacing NATS with `seidrum-eventbus` per PLAN.md L137-180.

3. **Phase 6** (interceptor-based plugin patterns + example plugins) follows after the kernel migration is stable.

The other "drift" items (dead-letter retention, `delivery_idx` shape, `RetryPolicy` per-subscription, default interceptor timeout) should be discussed as a batch and either fixed or have the spec updated to match the current design.

The "gap" items in observability, benchmarks, and chaos testing should be tracked but are not blockers for the kernel migration.
