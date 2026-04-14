# Phase 5 Steps 3-5: NATS → EventBus Cutover — Implementation Plan

**Status:** ✅ COMPLETE. Executed in PR #33.
**Branch:** `feat/eventbus-phase5-cutover` (to be created)
**Dependencies:** PR #31 merged (WsClient + BusClient backend dispatch)
**Estimated effort:** ~10-14 hours

---

## Current State

As of commit `f59d97f` on `main`:
- `async_nats` exists in **one Cargo.toml** only: `seidrum-common/Cargo.toml`
- `async_nats` code exists in **one source file** only: `seidrum-common/src/bus_client.rs`
- All 23 plugins + all kernel services already use `BusClient` exclusively
- `BusClient` already dispatches on URL scheme (`ws://` → WsClient, `nats://` → async_nats)
- Changing `nats://localhost:4222` to `ws://localhost:9000` in config is literally all plugins need

---

## Step 1: Add In-Process Backend to BusClient

**Why first:** The kernel should use `EventBus` directly (no WS round-trip to itself).

**Files:**
- `seidrum-common/Cargo.toml` — add `seidrum-eventbus` as a regular dependency
- `seidrum-common/src/bus_client.rs` — add `BackendHandle::InProcess(Arc<dyn EventBus>)` variant

**Changes:**
1. Add `InProcess` variant to `BackendHandle` enum
2. Add `BusClient::from_bus(bus: Arc<dyn EventBus>, source: &str) -> Self` constructor
3. Add `InProcess` match arms in `publish_bytes`, `request_bytes`, `subscribe`, `is_connected`
4. The `subscribe` arm bridges `eventbus::Subscription.rx` → `WsMessage` via a converter task (same pattern as the existing NATS bridge)

**Default subscribe opts:** `SubscriptionMode::Async`, `ChannelConfig::InProcess`, priority `10`, timeout `5s`, no filter

**Test:** unit test that creates InMemoryEventStore → EventBusBuilder → BusClient::from_bus → publish/subscribe round-trip

**Complexity:** M (~3-4h)

---

## Step 2: Kernel Startup Swap

**Files:**
- `seidrum-kernel/Cargo.toml` — add `seidrum-eventbus` dependency
- `seidrum-kernel/src/main.rs` — replace NATS connection with EventBusBuilder

**Changes in `main.rs`:**
```
BEFORE:
  let nats_client = BusClient::connect(&nats_url, "kernel").await?;

AFTER:
  let store = Arc::new(RedbEventStore::open(&db_path)?);
  let handles = EventBusBuilder::new()
      .storage(store)
      .with_websocket(ws_addr)
      .with_retry(RetryConfig::default())
      .unsafe_allow_ws_dev_mode()
      .build_with_handles().await?;
  let nats_client = BusClient::from_bus(handles.bus.clone(), "kernel");
```

**New env vars:** `EVENTBUS_WS_ADDR` (default `0.0.0.0:9000`), `EVENTBUS_DATA_DIR` (default `data`)

**Variable name `nats_client` kept temporarily** to minimize diff — rename in a follow-up.

**Test:** `cargo build -p seidrum-kernel` + `cargo run -p seidrum-kernel -- serve` starts without NATS

**Complexity:** M (~2-3h)

---

## Step 3: Config and Environment Migration

**Mechanical find-and-replace across 23+ files.**

### 3a. Plugin CLI args (23 files)
```
BEFORE: #[arg(long, env = "NATS_URL", default_value = "nats://127.0.0.1:4222")]
        nats_url: String,
        BusClient::connect(&args.nats_url, ...)

AFTER:  #[arg(long, env = "BUS_URL", alias = "nats-url", default_value = "ws://127.0.0.1:9000")]
        bus_url: String,
        BusClient::connect(&args.bus_url, ...)
```

### 3b. Platform config
- `config/platform.yaml`: `nats_url` → `eventbus_url: ws://localhost:9000`
- `seidrum-common/src/config.rs`: rename field, add `#[serde(alias = "nats_url")]`

### 3c. Docker-compose files
- `docker-compose.yml`: remove `nats:` service, add `EVENTBUS_WS_ADDR` to kernel, change plugin `NATS_URL` → `BUS_URL: ws://kernel:9000`
- `docker-compose.test.yml`: remove `nats:` service, add kernel service with `EVENTBUS_WS_ADDR`

### 3d. Daemon
- `daemon.rs`: `NATS_URL` → `BUS_URL`, default `ws://localhost:9000`

**Complexity:** S (~2-3h, tedious but mechanical)

---

## Step 4: Drop NATS Backend

**Files:**
- `seidrum-common/src/bus_client.rs` — remove `BackendHandle::Nats` variant + all match arms
- `seidrum-common/Cargo.toml` — remove `async-nats = "0.38"`

After this, `nats://` URLs error immediately. Only `ws://` and in-process work.

**Complexity:** S (~30min)

---

## Step 5: Daemon NATS Removal

**Files:** `seidrum-daemon/src/infra.rs`, `setup.rs`, `main.rs`, `daemon.rs`

Remove: `is_nats_reachable()`, `NatsConfig`, `download_nats()`, `start_nats()`, `stop_nats()`, NATS status display, NATS setup wizard steps.

**Complexity:** M (~1-2h)

---

## Step 6: E2E Test Migration

**Files:** `seidrum-e2e/tests/common/mod.rs` + all 6 test files

- `connect_nats()` → `connect_bus()`, env var `TEST_NATS_URL` → `TEST_BUS_URL`
- `nats_request()` → `bus_request()`
- Tests require a running kernel (not just NATS) — docker-compose.test.yml provides it

**Complexity:** S (~1h)

---

## Step 7: Documentation

- `CLAUDE.md` — NATS references → eventbus
- `docs/ARCHITECTURE.md` — if it mentions NATS
- `PHASE5_MIGRATION.md` — mark status complete
- `PLAN.md` — mark Phase 5 done

**Complexity:** S (~30min)

---

## Dependency Graph

```
Step 1 → Step 2 → Step 3 → Step 4 → Step 6 → Step 7
                     ↘ Step 5 → Step 7
```

## Verification Checkpoints

After each step: `cargo build --workspace` + `cargo test --workspace`

After Step 6: `docker compose -f docker-compose.test.yml up -d` + `cargo test -p seidrum-e2e -- --ignored`

## Total: ~10-14 hours across 7 steps
