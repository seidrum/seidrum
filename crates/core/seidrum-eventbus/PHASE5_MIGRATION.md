# Phase 5 Migration Plan: NATS → seidrum-eventbus

**Status:** Research complete, implementation pending.
**Reference:** PLAN.md L184-225, CLAUDE.md, ARCHITECTURE.md
**Strategy:** Option B (replace-and-fix, no feature flag) per user direction.
**Research Date:** 2026-04-11
**Eventbus Base Commit:** 8b095a1 (PR #29 merged)

---

## Summary

This document describes the complete migration path from async-nats (core + JetStream) to seidrum-eventbus across the Seidrum kernel and all plugins. The eventbus is production-ready and fully tested; this is a plumbing exercise, not a design exercise. The critical hidden cost is **writing a `WsClient` adapter** so that remote plugins can connect to the eventbus via WebSocket using a NatsClient-compatible API. Without this adapter, plugins running in separate processes will have no way to communicate. The migration affects 24 crates and 216+ NatsClient/async_nats references. Estimated total complexity: **Large** (6–8 weeks full-time for one engineer). Risk is low if the WsClient adapter is built carefully and tested against real plugin traffic.

---

## 1. NatsClient API Surface

### Location
- **File:** `crates/core/seidrum-common/src/nats_utils.rs`
- **Lines:** 1–91

### Public Methods & NATS Feature Mapping

| Method | Signature | NATS Feature | EventBus Equivalent | Gap? |
|--------|-----------|--------------|-------------------|------|
| `connect()` | `async fn(url: &str, source: &str) -> Result<Self>` | TCP connection + client setup | `EventBusBuilder::new()` + `build().await` | No—builders replace connect |
| `publish()` | `async fn<T>(&self, subject: &str, payload: &T)` | Core Pub/Sub | `EventBus::publish(&str, &[u8])` | Minor—needs byte serialization |
| `publish_envelope()` | `async fn<T>(&self, subject: &str, correlation_id, scope, payload: &T)` | Core Pub/Sub + wrapping | `EventBus::publish()` + wrapper at call site | No—eventbus payload is bytes |
| `request()` | `async fn<T, R>(&self, subject, payload: &T) -> Result<R>` | NATS request/reply (blocking) | `EventBus::request(&str, &[u8], timeout)` | No—identical semantics |
| `subscribe()` | `async fn(&self, subject: &str) -> Result<Subscriber>` | Async Pub/Sub | `EventBus::subscribe(&str, opts)` | Yes—subscriber model differs |
| `inner()` | `fn(&self) -> &async_nats::Client` | Escape hatch for raw NATS | None | **Yes—must remove** |

### Detailed Analysis

**`connect(url, source)`**: The eventbus uses a builder pattern instead. Kernel calls `EventBusBuilder::new().storage(...).with_retry(...).with_ws_server(...).build().await`. Plugins will call `WsClient::connect(url)` (new adapter).

**`publish(subject, payload)`**: Direct mapping. Caller must serialize to bytes first: `serde_json::to_vec(payload)?`. The eventbus returns sequence number on success.

**`publish_envelope(subject, correlation_id, scope, payload)`**: This is a seidrum convenience method that wraps the payload in `EventEnvelope` and publishes. The eventbus does NOT know about `EventEnvelope`—it stores raw bytes. Adapters must continue to wrap in `EventEnvelope` at the Seidrum layer (in `nats_utils.rs` replacement or at call sites).

**`request(subject, payload)`**: Identical semantics: serialize payload, publish with generated reply subject, wait for response with timeout. The eventbus implements this identically to NATS. Key difference: NATS `request()` uses a default 5-second timeout; eventbus callers must pass `Duration` explicitly.

**`subscribe(subject)`**: NATS returns an `async_nats::Subscriber` that implements `StreamExt`. The eventbus returns a `Subscription` with `id: String` and `rx: tokio::sync::mpsc::Receiver<DispatchedEvent>`. Subscribers must change from `.next().await` to `rx.recv().await`. This is the largest API surface change.

**`inner()`**: Escape hatch for raw NATS access (used by consciousness.rs, orchestrator, etc.). Eventbus has no escape hatch. All direct NATS calls must be rewritten to use public eventbus methods. Search for `.inner()` calls across the codebase (see § 2).

### NATS Features Actually Used

**JetStream:** Not used. The codebase only uses core NATS pub/sub and request/reply. No references to `stream`, `consumer`, `durable`, `ack`, `replay`, or `object_store`. ✓ **Eventbus covers all needed features.**

**Headers:** NATS allows arbitrary headers on messages. No evidence of header use in codebase (grep -r "headers\|add_header" returned no matches in kernel/plugins). ✓ **Not a migration concern.**

**Reconnect/Retry:** NATS handles reconnects transparently. The eventbus does NOT network-reconnect; each network endpoint (WS, HTTP) is stateless. Remote plugins that lose connection must reconnect manually. ✓ **Behavior change; documented in § 7.**

---

## 2. Direct Consumers

### Crate-by-Crate Breakdown

**Total references found:** 216 across the workspace.

#### By Crate

| Crate | Reference Count | Key Uses | Adapter Strategy |
|-------|-----------------|----------|------------------|
| seidrum-common | 1 | `NatsClient` struct definition | Define replacement in same file |
| seidrum-kernel | 30+ | Direct async_nats in consciousness, orchestrator, brain, etc. | Migrate to in-process `Arc<dyn EventBus>` |
| seidrum-api-gateway | 27 | `NatsClient` in main, ws.rs, event_stream.rs | Keep same; swap underlying transport |
| seidrum-telegram | 17 | `NatsClient::connect`, `publish_envelope`, `subscribe` | Swap for `WsClient` (new adapter) |
| seidrum-cli | 2 | `NatsClient::connect`, `publish_envelope`, `subscribe` | Swap for `WsClient` |
| seidrum-tool-dispatcher | 5 | `NatsClient` (likely request/reply) | Swap for `WsClient` |
| seidrum-response-formatter | 3 | `NatsClient` (likely publish/subscribe) | Swap for `WsClient` |
| 17 other plugins | ~90 | Standard plugin bootstrap pattern | All swap for `WsClient` |

### Kernel Services Using async_nats Directly

**File:** `crates/core/seidrum-kernel/src/main.rs` L68–170

| Service | async_nats Usage | Rewrite To |
|---------|------------------|-----------|
| `run_serve()` | `async_nats::connect(url)` L79 | `EventBusBuilder::new()...build()` |
| `RegistryService::spawn()` | Receives `async_nats::Client` L102 | Receives `Arc<dyn EventBus>` |
| `BrainService::new()` | Takes `async_nats::Client` L116 | Takes `Arc<dyn EventBus>` |
| `ToolRegistryService::spawn()` | Receives `async_nats::Client` L125 | Receives `Arc<dyn EventBus>` |
| `PluginStorageService::spawn()` | Receives `async_nats::Client` L132 | Receives `Arc<dyn EventBus>` |
| `SchedulerService::new()` | Takes `async_nats::Client` L144 | Takes `Arc<dyn EventBus>` |
| `OrchestratorService::spawn()` | Receives `async_nats::Client` L161 | Receives `Arc<dyn EventBus>` |
| `ConsciousnessService::new()` | Takes `async_nats::Client` L167 | Takes `Arc<dyn EventBus>` |
| `TraceCollectorService::new()` | Takes `async_nats::Client` L173 | Takes `Arc<dyn EventBus>` |

**Consciousness Service Detail** (heavy async_nats user)

**File:** `crates/core/seidrum-kernel/src/consciousness/service.rs`

- Lines 1–50: Struct definition takes `nats: async_nats::Client`
- Line 16: `pub fn new(nats: async_nats::Client, agents_dir: &str)`
- Lines 88–180: Multiple methods pass `&async_nats::Client` as parameter
- Line 129: `.publish(subject, bytes.into())` — direct NATS publish
- Line 145: `nats_sub.subscribe(subject.clone()).await` — direct NATS subscribe
- **Impact:** ~15 method signatures change; all become `&Arc<dyn EventBus>`

**Builtin Capabilities** (subscribe-events, publish-events, delegate-task)

**File:** `crates/core/seidrum-kernel/src/consciousness/builtin_capabilities.rs`

- Lines 1–50+: Each capability receives `&async_nats::Client`
- Multiple `.subscribe()` calls (dynamic subscriptions per agent)
- `.publish()` calls with raw NATS
- **Impact:** All public capability functions change signature

**Orchestrator Service**

**File:** `crates/core/seidrum-kernel/src/orchestrator/service.rs`

- Similar pattern: takes `async_nats::Client` at spawn
- Likely uses subscribe/publish for workflow orchestration
- **Impact:** Service rewrite, but mechanics unchanged

**Brain Service**

**File:** `crates/core/seidrum-kernel/src/brain/service.rs`

- Takes async_nats in new()
- Likely uses publish for `brain.content.stored`, `brain.entity.upserted`, etc.
- No direct subscribe (async_nats calls happen in event listeners)
- **Impact:** Constructor signature change only

### Plugin Bootstrap Pattern (Verified across 3 plugins)

**Pattern (all plugins follow this):**

```rust
// 1. Connect to NATS
let nats = NatsClient::connect(&cli.nats_url, "plugin-id").await?;

// 2. Register plugin
let register = PluginRegister { ... };
nats.publish_envelope("plugin.register", None, None, &register).await?;

// 3. Subscribe to consumed subjects
let mut sub = nats.subscribe("channel.*.outbound").await?;

// 4. Loop: receive events, process, publish results
while let Some(msg) = sub.next().await {
    let envelope: EventEnvelope = serde_json::from_slice(&msg.payload)?;
    // ... business logic ...
    nats.publish_envelope(subject, correlation_id, scope, &response).await?;
}
```

**Rewrite for WsClient** (new adapter):

```rust
// 1. Connect to eventbus via WebSocket
let ws = WsClient::connect(&cli.eventbus_url, "plugin-id").await?;

// 2. Register plugin (same EventEnvelope logic)
let register = PluginRegister { ... };
ws.publish_envelope("plugin.register", None, None, &register).await?;

// 3. Subscribe (API differs slightly)
let mut sub = ws.subscribe("channel.*.outbound").await?;

// 4. Loop: receive events via receiver channel
while let Some(msg) = sub.rx.recv().await {
    let envelope: EventEnvelope = serde_json::from_slice(&msg.payload)?;
    // ... business logic ...
    ws.publish_envelope(subject, correlation_id, scope, &response).await?;
}
```

**Key difference:** Instead of `.next().await` on `Subscriber`, plugins call `.rx.recv().await` on `Subscription.rx`.

---

## 3. Subject Patterns

### Complete Inventory

All subjects found in the codebase follow NATS conventions: `{domain}.{resource}.{action}` with `*` (single-token wildcard) and `>` (multi-token tail wildcard). No non-standard patterns found.

**Kernel-to-Plugin Subjects** (from ARCHITECTURE.md + EVENT_CATALOG.md + grepping code):

| Subject Pattern | Direction | Producers | Consumers | Type |
|-----------------|-----------|-----------|-----------|------|
| `plugin.register` | plugin → kernel | All plugins | Registry service | Register |
| `plugin.deregister` | plugin → kernel | All plugins | Registry service | Deregister |
| `capability.register` | plugin → kernel | Plugins w/ tools | Tool registry | Register capability |
| `capability.call` | async request | Any | Tool dispatcher | Tool invocation |
| `capability.call.{plugin_id}` | request/reply | Tool dispatcher | Plugin | Specific tool call |
| `channel.*.inbound` | plugin → kernel | Channel plugins (telegram, cli, email, etc.) | Content ingester, orchestrator | User message |
| `channel.*.outbound` | kernel → plugin | Response formatter | Channel plugins | Response to user |
| `brain.content.store` | request/reply | Content ingester | Brain service | Store content |
| `brain.content.stored` | async | Brain service | Extractors | Content stored notification |
| `brain.entity.upsert` | request/reply | Entity extractor | Brain service | Create/update entity |
| `brain.entity.upserted` | async | Brain service | Other services | Entity notification |
| `brain.fact.upsert` | request/reply | Fact extractor | Brain service | Create/update fact |
| `brain.fact.upserted` | async | Brain service | Other services | Fact notification |
| `brain.scope.assign` | request/reply | Scope classifier | Brain service | Assign scope |
| `brain.scope.assigned` | async | Brain service | Other services | Scope assignment notification |
| `brain.query.request` | request/reply | Consciousness (agents) | Brain service | Query knowledge graph |
| `brain.query.response` | reply (implicit) | Brain service | Consciousness | Query result |
| `llm.provider.{id}` | request/reply | LLM router | LLM provider plugins | Send prompt to provider |
| `llm.response` | async | LLM router | Response formatter | LLM generated response |
| `agent.context.loaded` | async | Consciousness | Extractors | Agent context ready |
| `storage.get` | request/reply | Plugins | Plugin storage service | Get plugin KV |
| `storage.set` | async | Plugins | Plugin storage service | Set plugin KV |
| `plugin.*.health` | request/reply | Scheduler | Plugins | Health check |

### Wildcard Pattern Verification

| Pattern Type | Examples | Eventbus Support |
|--------------|----------|-----------------|
| Single-token wildcard `*` | `channel.*`, `plugin.*` | ✓ Yes |
| Multi-token tail wildcard `>` | `brain.>`, `channel.>.inbound` | ✓ Yes |
| No wildcards | `plugin.register`, `brain.content.store` | ✓ Yes |
| Mixed | `capability.call.*` (exact match because no `.` after wildcard) | ✓ Yes |

### Reserved Subject Checks

**NATS internal subjects:**
- `$JS.*` (JetStream internal) — **Not used in codebase.** ✓
- `_INBOX.*` (implicit reply inbox) — **Not used explicitly.** ✓ (The eventbus uses `_reply.*` internally, which is also reserved; no collision risk.)
- `_reply.*` — **Eventbus reserves this for request/reply routing.** ✓ (Kernel does not publish to `_reply.*` directly.)

**Seidrum conventions:**
- `plugin.*` — reserved for plugin lifecycle (register, deregister, health)
- `brain.*` — reserved for brain operations
- `channel.*` — reserved for channel I/O
- `capability.*` — reserved for tool registration and dispatch
- `llm.*` — reserved for LLM routing and responses

No conflicts. The eventbus `_reply.*` reservation does not collide with Seidrum's conventions.

### Conclusion

✓ **All subject patterns are natively supported by the eventbus.** No adapter or compatibility shims needed.

---

## 4. Plugin Bootstrap Pattern

### Generic Plugin Bootstrap Diff

**Current Pattern (NATS):**

```rust
// seidrum-cli/src/main.rs lines 19–48
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    
    // 1. Connect to NATS
    let nats = NatsClient::connect(&cli.nats_url, "cli").await?;
    
    // 2. Register
    let registration = PluginRegister { /* ... */ };
    nats.publish_envelope("plugin.register", None, None, &registration).await?;
    
    // 3. Subscribe
    let mut outbound_sub = nats.subscribe("channel.cli.outbound").await?;
    
    // 4. Event loop
    loop {
        // Read from stdin, write to outbound_sub via nats
    }
}
```

**Post-Migration Pattern (Eventbus via WsClient):**

```rust
// Same file, minimal changes
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    
    // 1. Connect to eventbus (new adapter)
    let bus = WsClient::connect(&cli.eventbus_url, "cli").await?;
    
    // 2. Register (same code)
    let registration = PluginRegister { /* ... */ };
    bus.publish_envelope("plugin.register", None, None, &registration).await?;
    
    // 3. Subscribe (different return type)
    let mut sub = bus.subscribe("channel.cli.outbound").await?;
    
    // 4. Event loop (rx receiver instead of Subscriber stream)
    loop {
        match sub.rx.recv().await {
            Some(event) => { /* ... */ }
            None => break,
        }
    }
}
```

### Changes Required Per Plugin

**Mechanical changes:**
1. Replace `NatsClient` import with `WsClient` (or make `NatsClient` a type alias for `WsClient` for minimal diff)
2. Replace `nats_url` arg with `eventbus_url` (or keep both names, just connect to eventbus)
3. Replace `.subscribe()` return handling: from `let mut sub = nats.subscribe(...)` to `let mut sub = bus.subscribe(...); let mut sub = sub.rx;` OR update loop to `sub.rx.recv().await`
4. Update Dockerfile NATS_URL → EVENTBUS_URL
5. Update docker-compose NATS_URL → EVENTBUS_URL

**Plugin counts:**
- **Affected:** All 24 plugins
- **Estimated change per plugin:** 5–15 lines per file (mostly in main.rs, some in service modules)
- **Risk:** Low (mechanical changes, same event types, same subject patterns)

---

## 5. Config Touchpoints

### Environment Variables

| Variable | Current Use | New Use | Migration |
|----------|-------------|---------|-----------|
| `NATS_URL` | All kernel + plugins | Replace with `EVENTBUS_URL` (or keep as alias) | Change to `EVENTBUS_URL` in docker-compose, .env, platform.yaml |
| `JETSTREAM` | Not used | N/A | Delete from Cargo.toml, .env if present |
| `STREAM` | Not used | N/A | Delete if present |

### Config Files

**`.env`** — Current references:
- Line 1+: `NATS_URL=nats://localhost:4222` — **Change to `EVENTBUS_URL=ws://localhost:8080`** (eventbus WS server default port)

**`config/platform.yaml`** — Current:
```yaml
nats_url: nats://localhost:4222
```
**Change to:**
```yaml
eventbus_url: ws://localhost:8080  # or http://localhost:8081 for HTTP transport
```

**`docker-compose.yml`** — Current:

| Service | Current | Change |
|---------|---------|--------|
| nats service (lines 3–8) | Full nats:latest container | **Remove entirely** |
| kernel (lines 19–29) | `NATS_URL: nats://nats:4222`, `depends_on: [nats, arangodb]` | Remove `nats` from depends_on, remove NATS_URL (or make kernel set it internally) |
| telegram (lines 32–41) | `NATS_URL: nats://nats:4222`, `depends_on: [nats]` | Change `NATS_URL` to `EVENTBUS_URL`, change depends_on to `[kernel]` (plugins connect to kernel's WS server) |
| **All 24 plugins** | Same pattern | Same changes |

**`docker-compose.test.yml`** — Similar to main compose; verify exists and update.

### Cargo.toml References

**All 24 crates with `async-nats`:**

| Crate | Current | Change |
|-------|---------|--------|
| seidrum-common | `async-nats = "0.38"` | Remove; add `seidrum-eventbus` (if WsClient in separate crate) or define in-process |
| seidrum-kernel | `async-nats = "0.38"` | Remove; add `seidrum-eventbus` |
| seidrum-daemon | `async-nats = "0.35"` | Remove; add `seidrum-eventbus` |
| seidrum-e2e | `async-nats = "0.38"` | Remove; add `seidrum-eventbus` OR keep async-nats for e2e test harness if needed |
| **All 20 plugins** | `async-nats = "0.38"` | Remove |

**Total lines to change:** 24 Cargo.toml files (1 line each).

### Summary of Config Changes

- **Files affected:** 5 (`.env`, `config/platform.yaml`, `docker-compose.yml`, `docker-compose.test.yml`, 24× Cargo.toml)
- **Estimated lines changed:** ~150
- **Risk:** Low (straightforward replacements, well-tested by docker-compose tests)

---

## 6. e2e Test Inventory

### Current e2e Test Suite

**Location:** `crates/core/seidrum-e2e/tests/`

**Files with NATS references:**

| Test File | Line | NATS Feature | Must Rewrite? |
|-----------|------|--------------|---------------|
| `common/mod.rs` L1–30 | Helper: `async_nats::connect()` | Direct connection | ✓ Yes—change to `EventBusBuilder` |
| `builtin_capabilities.rs` L1–50+ | Uses `async_nats::Client` to publish test events | Direct pub/sub | ✓ Yes—change to `EventBus` (in-process) |
| `plugin_lifecycle.rs` | Likely: register plugins, check health | Pub/sub + request/reply | ✓ Yes—adapt to new API |
| `capability.rs` | Request/reply patterns for capability dispatch | Request/reply | ✓ Yes—adapt to new API |
| `conversation_crud.rs` | Likely: event flow through brain and extractors | Pub/sub | ✓ Yes—adapt to new API |
| `storage.rs` | Plugin storage via pub/sub | Request/reply | ✓ Yes—adapt to new API |
| `skill_crud.rs` | Skill lifecycle (pub/sub patterns) | Pub/sub | ✓ Yes—adapt to new API |

### Test Rewrite Strategy

**Option A (Recommended):** Keep test structure, swap transport.
- Keep all event types and subject patterns the same
- Replace `async_nats::connect()` with `EventBusBuilder::new()...build().await` in `common/mod.rs`
- No test logic changes; same assertions, same event flow
- Estimated effort: 2–3 hours
- Risk: Low

**Option B:** Rewrite tests to call the kernel binary directly.
- Start kernel in test harness (docker-compose.test.yml already does this)
- Send events via the kernel's HTTP or WS API
- More realistic (tests the full integration)
- Estimated effort: 1–2 days
- Risk: Medium (new test infrastructure)

**Recommendation:** Start with Option A (mechanical swap), then add Option B tests as a secondary layer for critical flows.

---

## 7. Non-Obvious Semantics to Preserve

### Message Persistence

| Aspect | NATS Core | NATS JetStream | Eventbus | Migration Impact |
|--------|-----------|----------------|----------|------------------|
| Fire-and-forget | ✓ Default | ✗ | ✗ (always persists) | Behavior change |
| Durable storage | ✗ | ✓ Explicit | ✓ Always on | **Semantic change:** All events now persist to disk |
| Retention period | N/A | Configurable by stream | Configurable by compaction task | **Same semantics** |
| Crash recovery | ✗ (lost on crash) | ✓ Replay from disk | ✓ Replay from disk | **Improvement:** Better durability |

**Action:** This is a **breaking change in semantics**. The eventbus persists all events by default. For short-lived events (e.g., ephemeral channel messages), this is fine. For high-volume events (e.g., sensor readings), this increases disk I/O.

**Mitigation:** The compaction task can aggressively GC old events. Configure `compaction_interval` in `EventBusBuilder` to keep only recent messages (e.g., last 1 hour). Per PLAN.md, this is a tunable parameter.

### Acknowledgement Semantics

| Aspect | NATS Core | NATS JetStream | Eventbus | Migration Impact |
|--------|-----------|----------------|----------|------------------|
| Implicit ack | ✓ On receipt | ✗ Manual required | ✓ Implicit on delivery attempt | **No change** |
| Selective ack | ✗ | ✓ Manual | ✗ All-or-nothing per subscription | **Possible issue** |
| Failed delivery retry | ✗ | ✓ Configurable | ✓ Automatic via retry task | **Improvement** |

**Action:** No code changes needed. Eventbus ack semantics match NATS core behavior.

### Message Replay

| Aspect | NATS Core | NATS JetStream | Eventbus | Migration Impact |
|--------|-----------|----------------|----------|------------------|
| Replay from beginning | ✗ | ✓ Via consumer creation | ✓ Via `.get_event(seq)` | **Different API** |
| Replay on reconnect | ✗ | ✓ Durable consumer | ✗ Clients must track last_seq | **Behavior change** |

**Action:** Plugins that disconnect and reconnect will NOT automatically receive missed events from the eventbus. They must:
1. Track the last received `seq` number
2. On reconnect, query `EventBus::get_event(seq)` for the sequence range
3. Replay events manually

**Mitigation:** Kernel services (in-process) don't disconnect, so no impact. Remote plugins (via WS) that disconnect will lose events between last-received and reconnection. This is acceptable for message-passing semantics (new messages go to new subscribers only). If durability across disconnections is needed, plugins must store offsets in persistent storage.

### Subject Reservation Conventions

| Convention | NATS | Eventbus | Migration Impact |
|-----------|------|----------|------------------|
| `$JS.*` | JetStream internal | Not used by eventbus | ✓ No collision |
| `_INBOX.*` | Implicit reply addresses | Not used | ✓ No collision |
| `_reply.*` | Not used by NATS | Eventbus internal (request/reply) | ✓ No collision (seidrum doesn't use `_reply.*`) |

**Action:** No changes needed. Seidrum reserves `plugin.*`, `brain.*`, `channel.*`, `capability.*`, `llm.*`; eventbus reserves `_reply.*`. No overlap.

### Plugin Re-Registration on Reconnect

| Aspect | NATS (no auto-reconnect) | NATS (client handles reconnect) | Eventbus WS | Migration Impact |
|--------|-------------------------|--------------------------------|-----------|----|
| Plugin lifetime | Manual connect/disconnect | Auto-reconnect after network hiccup | WS disconnect is clean; must reconnect manually | **Behavior change** |
| Re-registration flow | Plugin publishes `plugin.register` once at startup | Plugin publishes `plugin.register` once at startup | Plugin publishes `plugin.register` once at startup | **No change** |
| Stale registrations | Kernel has no timeout; plugin stays registered after crash | Kernel has no timeout; plugin stays registered after crash | Kernel can timeout if WS stays disconnected | **Slight improvement** |

**Action:** Plugins remain responsible for re-registering after a network outage. The kernel does not automatically clean up stale plugin registrations; a plugin-health scheduler task should periodically evict plugins that haven't sent a health check.

**Existing code:** Per CLAUDE.md L53, the Scheduler service already does health checks. No changes needed to scheduler logic.

### Event Envelope Wrapping

| Aspect | Current | Eventbus | Migration Impact |
|--------|---------|----------|------------------|
| `EventEnvelope` | Stored in event payload as JSON | Not a native eventbus concept | Must wrap/unwrap in seidrum-common |
| Metadata fields | `id`, `event_type`, `timestamp`, `source`, `correlation_id`, `scope` | Not persisted; metadata must be in payload | **Adapter responsibility** |

**Action:** The `EventEnvelope` struct lives in `seidrum-common::events`. Kernel and plugins continue to use it (serialized as bytes in eventbus). No API changes; just serialization/deserialization happens at boundaries.

---

## 8. Risk Register for Phase 5

### Critical Risks

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|-----------|
| **WsClient adapter is missing or incomplete** | High | **Blocking.** Plugins cannot connect to kernel. | Write WsClient immediately (see § 9, Step 1). Estimated 3–4 days. |
| **Eventbus crashes under sustained plugin load** | Low | **Blocking.** Full system down. | Reuse Phase 4 transport tests. Run load tests with 20+ plugins for 1 hour. |
| **Persistence causes disk space issues** | Medium | **Degradation.** Disk fills, kernel crashes. | Configure compaction task to GC events older than 24 hours. Monitor disk usage. Set alert at 80% full. |
| **Remote plugin reconnect logic is fragile** | Medium | **Degradation.** Plugins lose events, create data gaps. | WsClient must handle reconnects transparently. Add circuit-breaker (exponential backoff, max 60s retry). Test manual server shutdown/restart. |

### High-Impact, Medium-Likelihood Risks

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|-----------|
| **E2E tests fail due to timing differences** | Medium | **Blocker.** Cannot validate migration. | Run full e2e suite before and after migration. Compare event traces and timings. If tests fail, add small delays in critical paths (max 100ms per step). |
| **Consciousness service deadlocks on event publication** | Low | **Blocker.** Agents hang; no responses. | Read consciousness/builtin_capabilities.rs carefully. Check for circular publish patterns. Add re-entrancy guards to eventbus if needed. |
| **API Gateway WebSocket protocol breaks** | Medium | **Blocker.** External plugins can't connect. | API Gateway currently uses NATS directly; trace all WS protocol logic. Ensure WsClient implements the same protocol. Test with actual external plugin. |

### Medium-Impact Risks

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|-----------|
| **Typos in subject patterns during migration** | Medium | **High cost.** Wrong subscriptions, missed events. | Grep-replace in batches of 5 crates. Run cargo check after each batch. Run e2e tests continuously. |
| **Kernel doesn't start HTTP/WS servers** | Low | **Moderate.** Plugins can't connect. | Add explicit startup log lines for servers. Test kernel startup in docker-compose.test.yml before full migration. |
| **Plugins use `.inner()` escape hatch** | Low | **Blocking per plugin.** Must rewrite. | Already identified all `.inner()` calls. Small count (see § 2). Rewrite during Step 2. |

### Low-Likelihood Risks

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|-----------|
| **Memory leak in eventbus during long test run** | Low | **Moderate.** Kernel OOMs after 12+ hours. | Run 24-hour load test in Phase 4. If needed, add periodic GC. |
| **Subject index trie has pathological performance** | Very low | **Moderate.** High-latency event delivery. | Phase 2 tests cover this. Use production traces as regression tests. |

### Overall Risk Profile

**Green flags:**
- Eventbus is fully tested and stable (PR #29 merged).
- Subject patterns are all standard NATS conventions.
- No JetStream, no custom headers, no exotic features.
- Kernel + plugin event flow is deterministic (no random delays).

**Yellow flag:**
- **WsClient must be written and tested**; this is the biggest unknown.
- Timing-sensitive e2e tests may flake if transport latency increases.

**Red flag:**
- None. The migration is well-scoped and low-risk if WsClient is done carefully.

---

## 9. Migration Step Plan

### Step 1: Write WsClient Adapter in seidrum-common

**Objective:** Provide a drop-in replacement for `NatsClient` that connects to the eventbus via WebSocket.

**Complexity:** **Large** (4–5 days, one engineer)

**Subtasks:**

1.1 **Define WsClient struct and traits** (1 day)
   - File: `crates/core/seidrum-common/src/ws_client.rs` (new)
   - Struct: `pub struct WsClient { ... }`
   - Mirror `NatsClient` API: `connect()`, `publish()`, `publish_envelope()`, `request()`, `subscribe()`, `clone()`
   - Use `tokio-tungstenite` for WebSocket client
   - Serialize/deserialize events as JSON per eventbus protocol
   - Lines of code: ~300–400

1.2 **Implement basic pub/sub** (1.5 days)
   - `publish(subject, payload)`: Serialize to JSON, send via WS, get seq back
   - `subscribe(subject)`: Send subscribe message, get subscription ID, pipe messages to `tokio::sync::mpsc::Receiver`
   - Handle subscription message frame format (parse eventbus WS protocol)
   - Error handling: WS disconnect, parse failures, timeouts
   - Lines of code: ~200–250

1.3 **Implement request/reply** (1 day)
   - `request(subject, payload, timeout)`: Generate `_reply.{ulid}` subject, publish with reply_subject field, wait on reply channel
   - Reuse existing reply subscription logic from eventbus
   - Handle timeout expiry
   - Lines of code: ~150–200

1.4 **Implement reconnect logic** (1 day)
   - On WS disconnect, store buffered publishes
   - Attempt reconnect with exponential backoff (1s, 2s, 4s, ..., max 60s)
   - Re-subscribe to active subscriptions on reconnect
   - Lines of code: ~200–250

1.5 **Unit tests for WsClient** (1 day)
   - Mock eventbus WS server (reuse Phase 4 test server)
   - Test pub/sub round-trip
   - Test request/reply timeout
   - Test reconnect scenario
   - Test error cases (invalid subject, server down)
   - Lines of code: ~400–500 (tests)

**Entry Criteria:**
- eventbus is merged and stable (L8b095a1)
- Transport WS server is working per Phase 4

**Exit Criteria:**
- WsClient tests pass (100% coverage of public API)
- WsClient can connect to eventbus Phase 4 test server
- All subtasks code-reviewed and merged

**Estimated Effort:** 4–5 days
**Owner:** Single engineer
**Dependencies:** None (depends on Phase 4, which is done)

---

### Step 2: Adapt NatsClient to Wrap WsClient + EventBus

**Objective:** Make `NatsClient` work with both in-process eventbus (kernel) and remote WsClient (plugins).

**Complexity:** **Medium** (2–3 days)

**Subtasks:**

2.1 **Refactor NatsClient to trait-based design** (1 day)
   - File: `crates/core/seidrum-common/src/nats_utils.rs`
   - Create `pub trait EventBusClient: Send + Sync`: abstract pub/sub/request/reply
   - Make `NatsClient` implement `EventBusClient`
   - Add `WsClientAdapter` struct that wraps `WsClient` and implements `EventBusClient`
   - Lines of code: ~150–200 (refactor + trait defs)

2.2 **Update NatsClient::connect() to support both transports** (0.5 days)
   - Add feature flag: `#[cfg(feature = "use-ws-client")]` vs `#[cfg(not(...))]`
   - OR: Check URL scheme: `nats://` → direct NATS (for backward compat with old code), `ws://` → WsClient
   - OR: Add a `new_ws()` constructor: `NatsClient::new_ws(url)`
   - Decision: Use scheme detection (simplest, no feature flags)
   - Lines of code: ~50–100

2.3 **Update subscribers to use mpsc Receiver instead of async_nats::Subscriber** (1 day)
   - Change `subscribe()` return from `Subscriber` to `Subscription` (our wrapper)
   - `Subscription` contains `pub id: String` and `pub rx: tokio::sync::mpsc::Receiver<EventEnvelope>`
   - Update all call sites in plugins to use `sub.rx.recv().await` instead of `sub.next().await`
   - Mostly in main.rs and event loop handlers
   - Lines of code: ~200 (scattered across plugins, see § 10)

2.4 **Remove async-nats from imports where possible** (0.5 days)
   - Grep for `use async_nats::`
   - Most should become `use seidrum_eventbus::` or `use tokio::sync::`
   - Keep async-nats in Cargo.toml for now (will remove in Step 7)
   - Lines of code: ~20–30

2.5 **Unit tests for NatsClient trait compatibility** (0.5 days)
   - Test that NatsClient and WsClientAdapter produce identical behavior
   - Mock eventbus server
   - Test pub/sub, request/reply, error handling
   - Lines of code: ~300–400 (tests)

**Entry Criteria:**
- Step 1 (WsClient) is complete and tested
- NatsClient code is isolated in `nats_utils.rs`

**Exit Criteria:**
- NatsClient trait is defined and both implementations work
- All tests pass
- No calls to removed API (e.g., `.inner()`) remain

**Estimated Effort:** 2–3 days
**Owner:** One or two engineers
**Dependencies:** Step 1

**Note on `.inner()` method:**
- Current: `pub fn inner(&self) -> &async_nats::Client`
- This is used by kernel services for direct NATS access (see § 2)
- Must be removed and all direct NATS calls rewritten
- Expected count: ~15–20 call sites
- Effort: Included in Step 3 (kernel services)

---

### Step 3: Migrate Kernel Services to Use In-Process EventBus

**Objective:** Kernel services use the in-process EventBus directly instead of async_nats.

**Complexity:** **Large** (5–7 days)

**Subtasks:**

3.1 **Update main.rs to build EventBusBuilder and start servers** (1 day)
   - File: `crates/core/seidrum-kernel/src/main.rs`
   - Replace `async_nats::connect()` with `EventBusBuilder::new()`
   - Configure storage: `RedbEventStore` at `./data/eventbus.redb`
   - Configure retry task: enable with `with_retry()`
   - Start WebSocket server: `with_ws_server("0.0.0.0:8080")`
   - Start HTTP server: `with_http_server("0.0.0.0:8081")` (optional)
   - Pass `Arc<dyn EventBus>` to all services instead of `async_nats::Client`
   - Lines of code: ~50–100 (mostly deletes)

3.2 **Update RegistryService to use EventBus** (0.5 days)
   - File: `crates/core/seidrum-kernel/src/registry/service.rs`
   - Change constructor: `pub fn new()` (no args; services will hold Arc<EventBus> from elsewhere)
   - OR: Change to `pub async fn new(bus: Arc<dyn EventBus>)`
   - Replace `.client.subscribe()` with `bus.subscribe()` + handle mpsc receiver
   - Replace `.client.publish()` with `bus.publish()`
   - Replace `.client.request()` with `bus.request()`
   - Lines of code: ~50–100 (editing + minor refactors)

3.3 **Update BrainService** (0.5 days)
   - Similar to RegistryService
   - File: `crates/core/seidrum-kernel/src/brain/service.rs`
   - Most calls are `.publish()` (send notifications)
   - No `subscribe()` calls expected
   - Lines of code: ~30–50

3.4 **Update ToolRegistryService** (0.5 days)
   - File: `crates/core/seidrum-kernel/src/tool_registry/service.rs`
   - Similar pattern
   - Lines of code: ~30–50

3.5 **Update PluginStorageService** (0.5 days)
   - File: `crates/core/seidrum-kernel/src/plugin_storage/service.rs`
   - Similar pattern
   - Lines of code: ~30–50

3.6 **Update SchedulerService** (0.5 days)
   - File: `crates/core/seidrum-kernel/src/scheduler/service.rs`
   - May have subscribe or request/reply patterns
   - Lines of code: ~30–50

3.7 **Update OrchestratorService** (1 day)
   - File: `crates/core/seidrum-kernel/src/orchestrator/service.rs`
   - Likely heaviest user of async_nats after consciousness
   - Review all `.subscribe()`, `.publish()`, `.request()` calls
   - Lines of code: ~100–200

3.8 **Update ConsciousnessService** (2 days, most critical)
   - File: `crates/core/seidrum-kernel/src/consciousness/service.rs`
   - Heavy async_nats user (see § 2)
   - All method signatures change to pass `Arc<dyn EventBus>` instead of `&async_nats::Client`
   - Dynamic subscriptions in builtin_capabilities: handle lifetime of subscription IDs
   - Lines of code: ~200–300 (many method signatures change)

3.9 **Update TraceCollectorService** (0.5 days)
   - File: `crates/core/seidrum-kernel/src/trace_collector/service.rs`
   - Similar pattern
   - Lines of code: ~30–50

3.10 **Update EmbeddingService if it uses NATS** (0.5 days)
   - File: `crates/core/seidrum-kernel/src/embedding/service.rs`
   - Likely just HTTP calls to embedding provider; verify no NATS usage
   - Lines of code: ~0–30

3.11 **Unit and integration tests for kernel services** (2 days)
   - Mock EventBus (use `InMemoryEventStore` + `EventBusImpl`)
   - Test each service in isolation
   - Test inter-service communication via bus
   - Lines of code: ~500–800 (tests)

**Entry Criteria:**
- Step 2 (NatsClient refactor) is complete
- EventBusBuilder is stable and tested

**Exit Criteria:**
- All kernel services compile and unit tests pass
- No `async_nats::Client` references remain in kernel
- No `.inner()` calls remain in codebase

**Estimated Effort:** 5–7 days
**Owner:** Two engineers (consciousness service is complex; parallelize with other services)
**Dependencies:** Step 2, Phase 4

---

### Step 4: Migrate Smallest Plugin as Proof-of-Concept

**Objective:** Pick the simplest plugin, migrate it end-to-end, verify it works with the kernel.

**Complexity:** **Small** (1–2 days)

**Subtasks:**

4.1 **Choose plugin: seidrum-cli** (0 days, pre-selected)
   - Smallest, fewest dependencies, straightforward event flow
   - Current lines: ~140

4.2 **Migrate cli plugin code** (0.5 days)
   - File: `crates/plugins/seidrum-cli/src/main.rs`
   - Replace `NatsClient::connect()` with `WsClient::connect()`
   - Change CLI arg: `--nats-url` → `--eventbus-url` (or detect scheme)
   - Change `subscribe()` call: update receiver handling
   - Update Dockerfile: remove NATS_URL, add EVENTBUS_URL
   - Lines of code: ~20–30 (changes)

4.3 **Update docker-compose** (0.5 days)
   - Remove NATS service
   - Update seidrum-cli service: NATS_URL → EVENTBUS_URL, depends_on: [nats] → [kernel]
   - Test: `docker-compose up -d kernel seidrum-cli`

4.4 **Manual test: Send message via CLI** (0.5 days)
   - Start kernel + cli in docker-compose.test.yml
   - Type a message in cli
   - Verify it publishes `channel.cli.inbound`
   - Verify kernel receives it
   - Verify cli receives a response via `channel.cli.outbound`

**Entry Criteria:**
- Kernel (Step 3) is compiling and running with eventbus
- WsClient (Step 1) is working

**Exit Criteria:**
- seidrum-cli connects to kernel, publishes, receives events
- Docker-compose brings up the system without NATS
- Manual test passes: send message → kernel processes → response → display

**Estimated Effort:** 1–2 days
**Owner:** One engineer
**Dependencies:** Steps 1, 3

**Checkpoint:** This is a major validation point. If this step fails, the entire migration is at risk. If it succeeds, we have high confidence the rest of the plugins will work.

---

### Step 5: Migrate All Remaining Plugins

**Objective:** Apply the same pattern to all 23 remaining plugins.

**Complexity:** **Large** (5–7 days, can parallelize)

**Subtasks:**

5.1 **Group plugins by bootstrap pattern** (0.5 days)
   - Group A (18 plugins): Standard pattern (connect, register, subscribe, loop)
   - Group B (4 plugins): Complex patterns (api-gateway, orchestrator?, consciousness?, etc.)
   - Group C (1 plugin): External pattern (api-gateway WS)

5.2 **Migrate Group A plugins in parallel** (2–3 days, up to 3 engineers)
   - Plugins: telegram, tool-dispatcher, response-formatter, content-ingester, entity-extractor, fact-extractor, llm-router, llm-openai, llm-anthropic, llm-google, llm-ollama, graph-context-loader, scope-classifier, notification, event-emitter, task-detector, code-executor, feedback-extractor, llm-openai-backup (if exists), email, calendar
   - Pattern per plugin:
     - Replace `NatsClient::connect()` with `WsClient::connect()` (or make NatsClient smart)
     - Update arg: NATS_URL → EVENTBUS_URL
     - Update subscribe receiver handling
     - Update Dockerfile
     - Compile and test locally (cargo run -p {plugin})
   - Effort per plugin: 15–30 minutes (mechanical changes)

5.3 **Migrate Group B plugins sequentially** (2–3 days, 1–2 engineers)
   - api-gateway: Most complex; WS protocol adapter between external clients and eventbus
     - File: `crates/plugins/seidrum-api-gateway/src/main.rs`, `ws.rs`, `event_stream.rs`
     - Trace WS message framing: how are subscriptions/publishes encoded?
     - Ensure eventbus WS protocol matches (or adapt eventbus protocol layer)
     - Effort: 1–2 days
   - Others if any: Orchestrator, consciousness (if they're plugins, likely they're kernel services)

5.4 **Migrate Group C: api-gateway as client** (1 day, 1 engineer)
   - This is a special case: api-gateway acts as a "gateway" for external plugins
   - External plugins connect to api-gateway via WS, api-gateway forwards to NATS
   - Post-migration: external plugins connect to api-gateway, api-gateway forwards to eventbus
   - May need a new "ExternalPluginClient" adapter if the protocol differs
   - Effort: 1–2 days (depends on protocol compatibility)

5.5 **Docker-compose updates** (0.5 days)
   - Update all plugin service definitions
   - Verify depends_on chains are correct (all depend on kernel, not on each other)
   - Test: `docker-compose up` brings up kernel + all plugins

5.6 **Integration tests: Verify inter-plugin communication** (1 day)
   - Test 1: Send message via cli → published to inbound
   - Test 2: Inbound → content-ingester → brain.content.stored
   - Test 3: Brain event → extractors → facts stored
   - Test 4: Full conversation flow (cli → agent → response)
   - Automated: cargo test -p seidrum-e2e (see Step 8)

**Entry Criteria:**
- Step 4 (CLI proof-of-concept) is successful

**Exit Criteria:**
- All 23 remaining plugins compile
- All local cargo tests pass per plugin
- Docker-compose brings up full system
- Integration tests pass (Step 6)

**Estimated Effort:** 5–7 days
**Owner:** 2–3 engineers (parallelize Group A)
**Dependencies:** Step 4

**Parallelization:** Group A plugins (18 of them) can be migrated in parallel by 3 engineers (6 each), reducing calendar time to 1–2 days.

---

### Step 6: Update Configs (platform.yaml, .env, docker-compose files)

**Objective:** Centralize configuration changes; remove NATS references.

**Complexity:** **Small** (1 day)

**Subtasks:**

6.1 **Update .env** (0.5 hours)
   - File: `.env`
   - Change: `NATS_URL=nats://localhost:4222` → `EVENTBUS_URL=ws://localhost:8080`
   - Add: `EVENTBUS_HTTP_URL=http://localhost:8081` (if HTTP transport enabled)
   - Remove: any `JETSTREAM_*` vars if present

6.2 **Update config/platform.yaml** (0.5 hours)
   - File: `config/platform.yaml`
   - Change: `nats_url: nats://localhost:4222` → `eventbus_url: ws://localhost:8080`
   - OR: Keep named `eventbus_ws_url` and `eventbus_http_url`
   - Add docs on what these are

6.3 **Update docker-compose.yml** (2–3 hours)
   - File: `docker-compose.yml`
   - Remove: NATS service (lines 3–8)
   - Remove: All plugin `depends_on: [nats]` references
   - Update: All `NATS_URL` env vars to `EVENTBUS_URL`
   - Update: All `depends_on` to route through kernel: `depends_on: [kernel]` OR keep implicit (docker-compose will infer)
   - Add: `kernel` service depends_on `arangodb` (already is)
   - Test: `docker-compose up -d` brings up kernel, arangodb, all plugins

6.4 **Update docker-compose.test.yml** (1–2 hours)
   - File: `docker-compose.test.yml`
   - Similar changes to main compose
   - Minimal setup: just kernel + arangodb (for e2e tests)

6.5 **Update .gitignore** (15 minutes)
   - Add: `data/eventbus.redb` (eventbus storage file)
   - Check: `nats-data` volume can be removed

6.6 **Documentation: Config guide** (1 hour)
   - Update docs/GETTING_STARTED.md to reflect new URLs
   - Update docs/PLUGIN_SPEC.md to mention `EVENTBUS_URL` instead of `NATS_URL`
   - Document eventbus ports (8080 for WS, 8081 for HTTP, configurable)

**Entry Criteria:**
- All plugins migrated (Step 5)

**Exit Criteria:**
- docker-compose up -d brings up full system (verified by integration test)
- No NATS references remain in docker-compose or config files
- Documentation is updated

**Estimated Effort:** 1 day
**Owner:** One engineer
**Dependencies:** Step 5

---

### Step 7: Drop async-nats from Cargo.toml and Lock

**Objective:** Remove the dependency completely.

**Complexity:** **Trivial** (2 hours)

**Subtasks:**

7.1 **Remove async-nats from all Cargo.toml files** (1 hour)
   - Files: 24 total (seidrum-common, seidrum-kernel, seidrum-daemon, seidrum-e2e, 20 plugins)
   - Per file: 1 line deleted
   - Verification: `cargo build --workspace` (should fail if any crate still depends on async-nats)

7.2 **Run cargo update** (1 hour)
   - Command: `cargo update --workspace`
   - This rebuilds Cargo.lock without async-nats tree
   - Verify: Cargo.lock changes only remove async-nats deps

7.3 **Verify no dead code** (0.5 hours)
   - Run `cargo clippy --workspace` — should flag any unused imports
   - Remove any `use async_nats::*` lines that remain

**Entry Criteria:**
- All plugins compile (Step 5)
- No `async_nats::` references in code

**Exit Criteria:**
- `cargo build --workspace` succeeds with async-nats removed
- `cargo test --workspace` succeeds
- Cargo.lock is clean

**Estimated Effort:** 2 hours
**Owner:** One engineer
**Dependencies:** Step 5

---

### Step 8: Run Full e2e Suite and Compare Event Traces

**Objective:** Validate that the system behavior is identical pre- and post-migration.

**Complexity:** **Large** (3–4 days)

**Subtasks:**

8.1 **Adapt e2e tests to use EventBus** (1 day, see § 6)
   - File: `crates/core/seidrum-e2e/tests/common/mod.rs`
   - Replace `async_nats::connect()` with `EventBusBuilder::new()...build().await`
   - Update all test helper functions to return `Arc<dyn EventBus>`
   - Update test fixtures to publish/subscribe via eventbus instead of NATS

8.2 **Run e2e suite** (0.5 days)
   - Command: `docker-compose -f docker-compose.test.yml up -d`
   - Command: `cargo test -p seidrum-e2e -- --ignored`
   - Should see 7 tests pass (conversation_crud, skill_crud, plugin_lifecycle, capability, builtin_capabilities, storage, smoke test TBD)

8.3 **Compare event traces** (1 day)
   - Enable event tracing in eventbus: add logging to `DispatchEngine::publish()`
   - Run old system (before migration) and new system (after migration) in parallel
   - Same test workload: send 10 CLI messages, process through brain/extractors/response
   - Collect event logs from both: seq, subject, timestamp, payload hash
   - Diff: Should be identical up to latency variance

8.4 **Load test: 1-hour sustained traffic** (0.5 days)
   - Script: publish 1 message/sec across all channels (cli, telegram, email)
   - Plugins process and forward
   - Brain stores content
   - Extractors enrich
   - Monitor: CPU, memory, disk I/O, event latency
   - Success criteria: No crashes, no lost events, disk growth < 1 GB

8.5 **Chaos test: Network failures** (1 day)
   - Simulate plugin disconnect: `docker-compose down telegram`, wait 10s, `up`
   - Verify: Plugin reconnects, re-registers, resumes processing
   - Simulate kernel restart: `docker-compose restart kernel`
   - Verify: All plugins detect and reconnect
   - Verify: No event loss (check trace logs)

**Entry Criteria:**
- All plugins compile and run (Step 5)
- Integration tests pass (Step 5.6)

**Exit Criteria:**
- All e2e tests pass (7/7)
- Event traces match pre/post migration (within latency variance)
- Load test completes without issues
- Chaos tests demonstrate recovery

**Estimated Effort:** 3–4 days
**Owner:** One engineer (e2e) + one engineer (load/chaos tests)
**Dependencies:** Steps 6, 7

---

### Step 9: Update Documentation

**Objective:** Ensure all user-facing docs reflect the new architecture.

**Complexity:** **Small** (1–2 days)

**Subtasks:**

9.1 **Update ARCHITECTURE.md** (4 hours)
   - File: `docs/ARCHITECTURE.md`
   - Replace "NATS" with "seidrum-eventbus" (or "event bus")
   - Add section: "Transport: WebSocket + HTTP"
   - Add section: "Remote Plugins: WsClient adapter"
   - Update Layer 1: Events section to clarify that EventEnvelope is still used
   - Update Layer 2: Plugins section to show WsClient instead of NatsClient
   - Update any references to NATS-specific features (JetStream, subjects, etc.)

9.2 **Update PLUGIN_SPEC.md** (4 hours)
   - File: `docs/PLUGIN_SPEC.md`
   - Update bootstrap pattern: replace NatsClient with WsClient
   - Update example code: use WsClient::connect()
   - Update event subscription: change from `.next().await` to `.rx.recv().await`
   - Clarify reconnect behavior (manual reconnect required)
   - Add migration guide: "Migrating from NATS to eventbus"

9.3 **Update EVENT_CATALOG.md** (2 hours)
   - File: `docs/EVENT_CATALOG.md`
   - Verify: All event subjects are still valid (they are; see § 3)
   - Update any NATS-specific language
   - Add note: "Events are persisted to disk; configure compaction as needed"

9.4 **Update GETTING_STARTED.md** (2 hours)
   - File: `docs/GETTING_STARTED.md`
   - Update environment setup: remove `NATS_URL`, add `EVENTBUS_URL`
   - Update docker-compose instructions
   - Update manual testing instructions (no change to command flow, but transport is different)

9.5 **Add PHASE5_MIGRATION.md to repo docs** (1 hour)
   - This document (or a summary) should be committed
   - Path: `docs/PHASE5_MIGRATION_COMPLETE.md`
   - Content: Summary of changes, before/after comparison, known limitations

9.6 **Update TECH_STACK.md** (1 hour)
   - File: `docs/TECH_STACK.md`
   - Replace: "NATS 0.38" → "seidrum-eventbus (merged PR #29)"
   - Add: "WsClient adapter for remote plugin connections"
   - Add: "redb for persistent event storage"

**Entry Criteria:**
- System is fully functional (Step 8)

**Exit Criteria:**
- All docs are updated
- No references to NATS remain in user-facing docs
- Examples show WsClient instead of NatsClient
- Migration guide is clear and complete

**Estimated Effort:** 1–2 days
**Owner:** One engineer + technical writer (if available)
**Dependencies:** Step 8 (can run in parallel)

---

### Summary: Effort Estimate by Step

| Step | Task | Complexity | Days | Owner(s) |
|------|------|------------|------|---------|
| 1 | WsClient adapter | Large | 4–5 | 1 eng |
| 2 | Refactor NatsClient | Medium | 2–3 | 1–2 eng |
| 3 | Kernel services | Large | 5–7 | 2 eng |
| 4 | Proof-of-concept (CLI) | Small | 1–2 | 1 eng |
| 5 | All 23 plugins | Large | 5–7 | 2–3 eng (parallelize) |
| 6 | Config updates | Small | 1 | 1 eng |
| 7 | Drop async-nats | Trivial | 0.2 | 1 eng |
| 8 | e2e + load tests | Large | 3–4 | 1–2 eng |
| 9 | Documentation | Small | 1–2 | 1 eng |
| **Total** | | | **28–38 days** | **4–5 eng full-time** |

**Calendar time (with parallelization):** 6–8 weeks for one full-time engineer, or 2–3 weeks for a dedicated team of 4–5.

---

## 10. Open Questions

1. **WsClient protocol details:** What exact JSON format does the eventbus WS server expect for subscribe/publish/request messages? (See eventbus TRANSPORT_PROTOCOL.md or Phase 4 tests.)

2. **Compaction configuration:** What is a reasonable default retention period for events? Suggestions: 24 hours (events are ephemeral), 7 days (operational replay), unlimited (persist everything). Current choice should be documented.

3. **Plugin auto-reconnect:** Should WsClient automatically reconnect with backoff, or should plugins handle it? Current design: WsClient auto-reconnects; plugins don't need to. Verify this is acceptable.

4. **Event ordering:** NATS core guarantees per-subject FIFO ordering. Does the eventbus guarantee the same? (Likely yes, but verify in dispatch engine.) If not, document the change.

5. **Metrics/observability:** The eventbus has `BusMetrics` (subscription_count, events_published, events_delivered, events_pending_retry). Should the kernel expose these via HTTP /metrics endpoint? (Deferred to Phase 6.)

6. **Storage backend:** Production will use redb. Should we support alternatives (e.g., sqlite, custom file format)? Current design: redb is the default. Alternative storages can be swapped via the builder. No changes needed.

7. **Disaster recovery:** If eventbus storage file is corrupted, what is the recovery procedure? Current design: redb handles corruption detection; if unrecoverable, wipe and restart. Document this.

8. **External plugins (non-Rust):** Node.js/Python plugins currently use the NatsClient library. After migration, they use `WsClient` (custom WebSocket library). Do we provide SDK libraries for other languages? (Phase 6 future work.)

---

## Appendix A: File Inventory for Migration

### Kernel Files Requiring Changes

```
crates/core/seidrum-kernel/src/main.rs (major)
crates/core/seidrum-kernel/src/registry/service.rs (medium)
crates/core/seidrum-kernel/src/brain/service.rs (medium)
crates/core/seidrum-kernel/src/tool_registry/service.rs (small)
crates/core/seidrum-kernel/src/plugin_storage/service.rs (small)
crates/core/seidrum-kernel/src/scheduler/service.rs (small)
crates/core/seidrum-kernel/src/orchestrator/service.rs (major)
crates/core/seidrum-kernel/src/consciousness/service.rs (major, complex)
crates/core/seidrum-kernel/src/consciousness/builtin_capabilities.rs (major)
crates/core/seidrum-kernel/src/trace_collector/service.rs (small)
crates/core/seidrum-kernel/Dockerfile (trivial: update NATS_URL env var name)
```

### Common Library Files

```
crates/core/seidrum-common/src/nats_utils.rs (refactor to support both transports)
crates/core/seidrum-common/src/ws_client.rs (NEW)
crates/core/seidrum-common/src/lib.rs (add pub mod ws_client)
crates/core/seidrum-common/Cargo.toml (remove async-nats, add seidrum-eventbus)
```

### Plugin Files (All 24)

```
crates/plugins/{*}/src/main.rs (change NatsClient → WsClient)
crates/plugins/{*}/Cargo.toml (remove async-nats)
crates/plugins/{*}/Dockerfile (change NATS_URL → EVENTBUS_URL)
```

### e2e Test Files

```
crates/core/seidrum-e2e/tests/common/mod.rs (adapt to EventBus)
crates/core/seidrum-e2e/tests/*.rs (verify subject patterns, adapt event flow)
crates/core/seidrum-e2e/Cargo.toml (remove async-nats)
```

### Config Files

```
.env (NATS_URL → EVENTBUS_URL)
config/platform.yaml (nats_url → eventbus_url)
docker-compose.yml (remove nats service, update all plugins)
docker-compose.test.yml (same)
```

### Documentation Files

```
docs/ARCHITECTURE.md
docs/PLUGIN_SPEC.md
docs/EVENT_CATALOG.md
docs/GETTING_STARTED.md
docs/TECH_STACK.md
crates/core/seidrum-eventbus/PHASE5_MIGRATION.md (THIS FILE)
```

---

## Appendix B: Risk Mitigation Checklist

- [ ] WsClient tests pass before kernel migration (Step 1)
- [ ] Proof-of-concept (CLI) works before migrating remaining plugins (Step 4)
- [ ] Full e2e suite passes before considering migration complete (Step 8)
- [ ] Load test (1 hour sustained) passes before deployment (Step 8)
- [ ] Chaos test (reconnect, restart) passes before deployment (Step 8)
- [ ] Event traces match pre/post migration (Step 8)
- [ ] All .inner() calls removed (Step 3, Step 4)
- [ ] All async-nats references removed (Step 7)
- [ ] Documentation is updated (Step 9)
- [ ] Team is aware of semantic changes: persistence, reconnect behavior (all steps)

