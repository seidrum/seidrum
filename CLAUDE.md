# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

Seidrum is an event-driven personal AI agent platform. Rust kernel + NATS JetStream + ArangoDB knowledge graph + plugin architecture. Everything is a plugin — independent processes that connect via NATS, declare consumed/produced event types, and self-wire.

## Build Commands

```bash
cargo build --workspace          # Build everything
cargo clippy --workspace         # Lint
cargo test --workspace           # Unit tests
cargo test -p seidrum-kernel     # Tests for one crate
cargo test -p seidrum-common test_name  # Single test by name

# E2E tests (require running infra)
docker compose -f docker-compose.test.yml up -d
cargo test -p seidrum-e2e -- --ignored
cargo test -p seidrum-e2e test_name -- --ignored  # Single e2e test

# Run binaries
cargo run -p seidrum-kernel -- serve
cargo run -p seidrum-kernel -- init          # Initialize ArangoDB
cargo run -p seidrum-daemon                  # Unified CLI for managing kernel + plugins
cargo run -p seidrum-telegram                # Run a specific plugin
```

## Architecture

### Event Flow

1. Channels (telegram, cli, api-gateway) receive user input → publish `channel.{platform}.inbound`
2. Content-ingester stores in brain, emits `content.stored`
3. Extractors (entity, fact, scope, task) enrich the knowledge graph
4. Orchestrator/consciousness loads agent context → emits `llm.request`
5. LLM router assembles context + calls provider → emits `llm.response`
6. Response-formatter formats per channel → publishes `channel.{platform}.outbound`
7. Channel plugin delivers response to user

### Kernel Services (crates/core/seidrum-kernel)

The kernel is a single binary that owns all shared state. Services run as concurrent tokio tasks:

- **Registry** (`registry/`): Plugin registry. Listens on `plugin.register`, responds to `registry.query`.
- **Brain** (`brain/`): All ArangoDB access. Handles `brain.*` subjects (content, entities, facts, scopes, tasks, conversations, skills). Uses HTTP client wrapper, not a native driver.
- **Tool Registry** (`tool_registry/`): Capability registry. Listens on `capability.register/search/describe`.
- **Plugin Storage** (`plugin_storage/`): Per-plugin key-value store via `storage.get/set/delete/list`.
- **Orchestrator** (`orchestrator/`): Loads workflow/agent YAML, routes events, manages correlation_id → origin mapping for response routing.
- **Consciousness** (`consciousness/`): Agent runtime. Subscribes to events per agent config, assembles context, calls LLM, handles built-in capabilities (brain-query, subscribe-events, delegate-task, schedule-wake).
- **Embedding** (`embedding/`): Generates embeddings, loads system skills from `skills/*.yaml`.
- **Scheduler** (`scheduler/`): Cron for fact confidence decay and plugin health monitoring.

### Plugin Bootstrap Pattern

Every plugin follows the same pattern (see any `crates/plugins/*/src/main.rs`):

1. Connect to the bus via `BusClient::connect()`
2. Build `PluginRegister` with id, name, consumes/produces lists, health_subject
3. Publish to `plugin.register`
4. Subscribe to consumed subjects
5. Process events in loop

Plugins never access ArangoDB directly — all brain operations go through bus request/reply to kernel.

### Common Library (crates/core/seidrum-common)

- `events.rs`: All event structs (`EventEnvelope`, `ChannelInbound`, `LlmRequest`, `ContentStoreRequest`, etc.)
- `config.rs`: Config loading (platform.yaml, agents, workflows)
- `bus_client.rs`: `BusClient` with `publish_envelope()`, `request()`, `subscribe()`, `publish_bytes()`, `request_bytes()`, `reply_to()`

### Configuration Files

- `.env`: Secrets (API keys, tokens, passwords)
- `config/platform.yaml`: Kernel config (NATS URL, ArangoDB connection, directories)
- `agents/*.yaml`: Agent definitions (prompt, tools, scope)
- `workflows/*.yaml`: Workflow definitions (trigger, steps, routing)
- `prompts/*.md`: Tera-templated system prompts
- `skills/*.yaml`: System skill definitions

### Infrastructure

- `docker-compose.yml`: Full stack (NATS + ArangoDB + kernel + all plugins)
- `docker-compose.test.yml`: Minimal (NATS + ArangoDB only, for e2e tests)

## Documentation

Design docs in `docs/` — read the relevant doc before implementing:

- `ARCHITECTURE.md` — System architecture, component map, data flow
- `BRAIN_SCHEMA.md` — ArangoDB collections, edges, indexes, query patterns
- `EVENT_CATALOG.md` — Event types with payload schemas
- `PLUGIN_SPEC.md` — Plugin interface, bootstrap pattern, built-in plugins
- `AGENT_SPEC.md` — Agent YAML format and workflow integration
- `LLM_INTEGRATION.md` — LLM router, routing strategies, context assembly

## Conventions

- Every event struct must derive `Serialize, Deserialize, Debug, Clone`
- Use `tracing` for logging, never `println!` in library/service code
- Error handling: `anyhow::Result` for binaries, `thiserror` for library errors
- No `unwrap()` in production code — use proper error propagation
- All brain access from plugins goes through NATS request/reply to kernel
- Plugins register via `plugin.register` and deregister via `plugin.deregister`
- Capabilities register via `capability.register` with kind `"tool"`, `"command"`, or `"both"`
- Scope enforcement on every brain query — ScopeService filters by scope field
