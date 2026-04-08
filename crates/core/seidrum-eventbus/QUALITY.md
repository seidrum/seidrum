# QUALITY.md — seidrum-eventbus Testing and Quality Strategy

## Guiding Principle

This crate is replacing an externally maintained, battle-tested message
broker. Every behavior it claims to provide must be proven by a test. "It
works on my machine" is not acceptable for infrastructure that every event
in Seidrum flows through.

The goal is not 100% line coverage — it is 100% behavior coverage. Every
documented guarantee (durability, ordering, timeout, retry, delivery) has
at least one test that would fail if the guarantee were broken.

---

## Test Pyramid

```
                    ╱╲
                   ╱  ╲
                  ╱ E2E╲        Few, slow, high-confidence
                 ╱──────╲       Full bus with transports
                ╱ Integr. ╲     Layer interactions, real redb
               ╱────────────╲
              ╱    Unit       ╲  Fast, isolated, in-memory store
             ╱────────────────╲
```

### Unit Tests

Run with `cargo test -p seidrum-eventbus`. No external dependencies. No
network. No disk (use `InMemoryEventStore`). Target: sub-second execution
for the full unit suite.

Every module has its own `#[cfg(test)] mod tests` block. Unit tests verify
the logic of a single component in isolation.

### Integration Tests

Located in `tests/`. Use real `RedbEventStore` with a temp directory. May
start transport servers on ephemeral ports. Still no external dependencies
(no Docker, no NATS).

Integration tests verify that layers compose correctly: storage + dispatch,
dispatch + delivery channels, transport + bus.

### End-to-End Tests

Located in `tests/e2e/` or in `seidrum-e2e` (the existing e2e crate). These
test the bus as a black box through its public transports: connect via
WebSocket, publish, subscribe, verify delivery. These are the slowest tests
and run with `--ignored` flag.

---

## What Must Be Tested

### Storage Engine

| Behavior | Test | Type |
|----------|------|------|
| Event round-trip: append then query returns identical bytes | `storage::test_append_and_query` | Unit |
| Sequence numbers are monotonically increasing | `storage::test_seq_monotonic` | Unit |
| Status updates are reflected in queries | `storage::test_status_update` | Unit |
| Per-subscriber delivery tracking | `storage::test_delivery_recording` | Unit |
| `query_by_status` returns only matching status | `storage::test_query_by_status_filter` | Unit |
| `query_by_subject` with exact match | `storage::test_query_by_subject_exact` | Unit |
| `query_by_subject` with pattern match | `storage::test_query_by_subject_pattern` | Unit |
| `query_retryable` returns failed deliveries below max_attempts | `storage::test_query_retryable` | Unit |
| Compaction removes old delivered events | `storage::test_compaction_delivered` | Unit |
| Compaction preserves dead-lettered events within retention | `storage::test_compaction_preserves_deadletter` | Unit |
| Crash recovery: events in Pending status after reopen | `storage::test_crash_recovery` | Integration |
| Concurrent appends do not corrupt data | `storage::test_concurrent_appends` | Integration |
| redb performance: 10k appends under 1 second | `storage::bench_append_throughput` | Benchmark |

### Subject Index

| Behavior | Test | Type |
|----------|------|------|
| Exact subject match | `subject_index::test_exact_match` | Unit |
| `*` wildcard matches single token | `subject_index::test_star_wildcard` | Unit |
| `*` wildcard does not match zero or multiple tokens | `subject_index::test_star_wildcard_boundaries` | Unit |
| `>` wildcard matches one or more trailing tokens | `subject_index::test_gt_wildcard` | Unit |
| `>` wildcard does not match zero tokens | `subject_index::test_gt_wildcard_nonempty` | Unit |
| Mixed wildcards: `channel.*.inbound` | `subject_index::test_mixed_pattern` | Unit |
| Multiple subscriptions on same pattern | `subject_index::test_multiple_subs_same_pattern` | Unit |
| Unsubscribe removes from index | `subject_index::test_unsubscribe` | Unit |
| No match returns empty | `subject_index::test_no_match` | Unit |
| Lookup returns subscriptions sorted by priority | `subject_index::test_priority_ordering` | Unit |
| Performance: 1000 subscriptions, lookup under 100µs | `subject_index::bench_lookup` | Benchmark |

### Interceptor Chains

| Behavior | Test | Type |
|----------|------|------|
| Sync interceptors run in priority order | `interceptor::test_priority_order` | Unit |
| Sync interceptor can mutate payload | `interceptor::test_mutation` | Unit |
| Mutation propagates to next interceptor | `interceptor::test_mutation_propagation` | Unit |
| Mutation propagates to async subscribers | `interceptor::test_mutation_visible_to_async` | Unit |
| Drop signal aborts chain | `interceptor::test_drop_aborts` | Unit |
| Drop signal prevents async delivery | `interceptor::test_drop_prevents_async` | Unit |
| Pass leaves payload unchanged | `interceptor::test_pass_unchanged` | Unit |
| Timed-out interceptor is skipped | `interceptor::test_timeout_skip` | Unit |
| Timed-out interceptor does not stall chain | `interceptor::test_timeout_no_stall` | Unit |
| Panicking interceptor is caught, chain continues | `interceptor::test_panic_recovery` | Unit |
| Async subscribers run concurrently | `interceptor::test_async_concurrent` | Unit |
| Mixed sync/async: sync runs first, async sees final state | `interceptor::test_mixed_ordering` | Unit |
| Re-entrant publish from sync interceptor returns error | `interceptor::test_reentrant_error` | Unit |
| Empty chain (no sync interceptors): async subscribers receive original | `interceptor::test_no_sync_passthrough` | Unit |

### Event Filters

| Behavior | Test | Type |
|----------|------|------|
| FieldEquals matches | `filter::test_field_equals_match` | Unit |
| FieldEquals rejects | `filter::test_field_equals_reject` | Unit |
| FieldContains matches substring | `filter::test_field_contains` | Unit |
| Nested path (dot notation) | `filter::test_nested_path` | Unit |
| All requires all conditions | `filter::test_all_combinator` | Unit |
| Any requires at least one | `filter::test_any_combinator` | Unit |
| Missing field rejects (does not panic) | `filter::test_missing_field` | Unit |

### Request/Reply

| Behavior | Test | Type |
|----------|------|------|
| Basic round-trip: request returns reply payload | `request_reply::test_basic_roundtrip` | Unit |
| Timeout when no handler subscribed | `request_reply::test_timeout_no_handler` | Unit |
| Timeout when handler is slow | `request_reply::test_timeout_slow_handler` | Unit |
| Multiple concurrent requests on same subject | `request_reply::test_concurrent_requests` | Unit |
| Request through interceptor chain: handler sees mutated payload | `request_reply::test_through_interceptor` | Unit |
| Reply subject cleanup after completion | `request_reply::test_cleanup_on_complete` | Unit |
| Reply subject cleanup after timeout | `request_reply::test_cleanup_on_timeout` | Unit |

### Delivery Channels

| Behavior | Test | Type |
|----------|------|------|
| InProcess: deliver and receive bytes | `delivery::test_in_process_roundtrip` | Unit |
| InProcess: backpressure when channel full | `delivery::test_in_process_backpressure` | Unit |
| WebSocket: deliver to connected client | `delivery::test_ws_delivery` | Integration |
| WebSocket: disconnect fails delivery | `delivery::test_ws_disconnect_failure` | Integration |
| WebSocket: reconnect resumes pending deliveries | `delivery::test_ws_reconnect_resume` | Integration |
| Webhook: successful POST returns delivered | `delivery::test_webhook_success` | Integration |
| Webhook: 500 response returns failed | `delivery::test_webhook_server_error` | Integration |
| Webhook: connection refused returns failed | `delivery::test_webhook_connection_refused` | Integration |
| Webhook: custom headers are sent | `delivery::test_webhook_custom_headers` | Integration |
| Custom: delivery delegates to registered provider | `delivery::test_custom_channel_delegation` | Unit |
| Custom: unregistered channel type returns error | `delivery::test_custom_unregistered_error` | Unit |

### Retry Logic

| Behavior | Test | Type |
|----------|------|------|
| Failed delivery is retried after backoff | `retry::test_retry_after_backoff` | Integration |
| Exponential backoff increases per attempt | `retry::test_exponential_backoff` | Unit |
| Max attempts reached → dead letter | `retry::test_max_attempts_deadletter` | Integration |
| Successful retry updates status to Delivered | `retry::test_successful_retry` | Integration |
| Partial delivery: only failed subscribers retried | `retry::test_partial_retry` | Integration |
| Backoff jitter prevents thundering herd | `retry::test_jitter` | Unit |

### Transport Servers

| Behavior | Test | Type |
|----------|------|------|
| WebSocket: auth with valid API key | `transport::test_ws_auth_valid` | Integration |
| WebSocket: reject invalid API key | `transport::test_ws_auth_invalid` | Integration |
| WebSocket: subscribe and receive events | `transport::test_ws_subscribe_receive` | Integration |
| WebSocket: publish from remote client | `transport::test_ws_remote_publish` | Integration |
| WebSocket: request/reply from remote client | `transport::test_ws_remote_request_reply` | Integration |
| WebSocket: sync interceptor from remote client | `transport::test_ws_remote_interceptor` | Integration |
| WebSocket: disconnect cleans up subscriptions | `transport::test_ws_disconnect_cleanup` | Integration |
| HTTP: publish endpoint | `transport::test_http_publish` | Integration |
| HTTP: request endpoint with timeout | `transport::test_http_request` | Integration |
| HTTP: subscribe webhook endpoint | `transport::test_http_subscribe_webhook` | Integration |
| HTTP: health endpoint | `transport::test_http_health` | Integration |

### Bus-Level (Full Integration)

| Behavior | Test | Type |
|----------|------|------|
| Publish → persist → deliver end-to-end | `bus::test_publish_deliver` | Integration |
| Crash recovery: reopen bus, pending events re-dispatched | `bus::test_crash_recovery_full` | Integration |
| High-throughput: 10k events, all delivered, none lost | `bus::test_throughput_no_loss` | Integration |
| Multi-subscriber: each subscriber gets own copy | `bus::test_multi_subscriber` | Integration |
| Interceptor + filter + delivery channel composition | `bus::test_full_pipeline` | Integration |
| Builder: minimal config (in-memory, no transports) | `bus::test_builder_minimal` | Unit |
| Builder: full config (redb + ws + http) | `bus::test_builder_full` | Integration |
| Graceful shutdown: pending events flushed | `bus::test_graceful_shutdown` | Integration |
| Metrics report correct counts | `bus::test_metrics` | Integration |

---

## Testing Utilities

The crate provides public test utilities so that downstream crates
(seidrum-kernel, plugins) can write tests against the bus easily.

### `seidrum_eventbus::test_utils`

```rust
/// Create an in-memory bus with no transports. Starts instantly,
/// no disk, no ports. Perfect for unit tests.
pub async fn test_bus() -> Arc<dyn EventBus> { ... }

/// Create a bus with real redb storage in a temp directory.
/// For integration tests that need durability verification.
pub async fn test_bus_with_storage() -> (Arc<dyn EventBus>, TempDir) { ... }

/// Create a bus with WebSocket and HTTP transports on ephemeral ports.
/// Returns the bus and the addresses of the transports.
pub async fn test_bus_with_transports() -> TestBusHandle { ... }

/// Helper: publish an event and wait for it to be delivered to
/// a subscriber, with a timeout. Panics with a clear message on
/// timeout.
pub async fn publish_and_wait(
    bus: &dyn EventBus,
    subject: &str,
    payload: &[u8],
    timeout: Duration,
) -> Vec<u8> { ... }

/// Helper: subscribe, collect N events, return them.
pub async fn collect_events(
    bus: &dyn EventBus,
    pattern: &str,
    count: usize,
    timeout: Duration,
) -> Vec<Vec<u8>> { ... }

/// A recording interceptor that logs every event it sees.
/// Useful for verifying interceptor ordering and mutation.
pub struct RecordingInterceptor { ... }

/// A mock webhook server that records received requests.
/// Starts on an ephemeral port.
pub struct MockWebhookServer { ... }
```

---

## Property-Based Testing

The subject index and event filter modules are candidates for property-based
testing with `proptest`:

- **Subject index:** generate random subjects and patterns, verify that the
  trie produces the same matches as a brute-force scan of all subscriptions.
- **Event filters:** generate random JSON payloads and filter expressions,
  verify that filter evaluation matches a reference implementation.
- **Interceptor ordering:** generate random priority assignments and verify
  that execution order always matches sorted priority.

---

## Benchmarks

Located in `benches/` using `criterion`:

| Benchmark | What it measures | Target |
|-----------|-----------------|--------|
| `bench_append` | redb append throughput | >10k events/sec |
| `bench_publish_deliver` | End-to-end publish → in-process deliver | <100µs p99 |
| `bench_subject_lookup` | Trie lookup with 1000 subscriptions | <100µs |
| `bench_interceptor_chain` | 5-interceptor sync chain, no mutation | <500µs |
| `bench_ws_roundtrip` | Publish via WebSocket, deliver to in-process | <1ms p99 |
| `bench_concurrent_publish` | 100 concurrent publishers, 10k events each | No dropped events |

Benchmarks run in CI on every PR to detect performance regressions.

---

## Chaos and Fault Injection

For Phase 4+ (transport servers), we add targeted fault injection tests:

- **Slow interceptor:** inject a `sleep(10s)` into a sync interceptor,
  verify the timeout fires and the chain continues within the timeout + 100ms.
- **Crashing webhook:** mock server that returns 200 three times then
  closes the socket. Verify retry logic handles the transition.
- **Reconnecting WebSocket:** drop the WebSocket connection mid-delivery,
  verify the event enters the retry queue and is re-delivered on reconnect.
- **Storage full:** mock an `EventStore` that returns `Err` on `append()`.
  Verify `publish()` returns the error to the caller (event not silently
  lost).
- **Concurrent storm:** 1000 publishers, 100 subscribers, 50 interceptors,
  all running simultaneously for 10 seconds. Verify: no panics, no
  deadlocks, all events eventually delivered or dead-lettered, delivery
  counts match publish counts.

---

## Coverage Policy

- **Required before merge:** all tests pass, no new `#[ignore]` without a
  tracking issue.
- **Coverage target:** measured but not gated. We care about behavior
  coverage (every guarantee has a test), not line percentage. A module with
  80% line coverage but missing an edge case test is worse than one with
  60% line coverage that tests every documented behavior.
- **Mutation testing:** run `cargo-mutants` periodically (not on every PR)
  to find behaviors that no test would catch if broken. Fix the gaps.

---

## CI Integration

```yaml
# Runs on every PR
test-unit:
  cargo test -p seidrum-eventbus          # Unit + integration
  cargo clippy -p seidrum-eventbus        # Lint
  cargo doc -p seidrum-eventbus --no-deps # Docs compile

# Runs on merge to main
test-full:
  cargo test -p seidrum-eventbus -- --ignored   # E2E tests
  cargo bench -p seidrum-eventbus               # Benchmarks (detect regressions)
```

---

## Tracing and Debugging

All event bus operations emit `tracing` spans and events at appropriate
levels:

- `INFO`: bus started, transport listening, subscription added/removed
- `DEBUG`: event published (subject, seq), delivery started, delivery
  completed
- `WARN`: interceptor timed out, delivery failed (will retry), channel
  unhealthy
- `ERROR`: storage write failed, max retries exhausted (dead-lettered),
  transport server error

Tests can enable `tracing-subscriber` with `RUST_LOG=seidrum_eventbus=debug`
to get full event flow traces for debugging failures.
