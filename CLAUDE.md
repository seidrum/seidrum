# Seidrum — Build Instructions

## What This Is

Seidrum is an event-driven personal AI agent platform. Rust kernel + NATS JetStream + ArangoDB knowledge graph + plugin architecture.

## Documentation

Design docs live in `docs/`. Read the relevant doc before implementing any task:

- `docs/PROJECT.md` — Vision, principles, glossary
- `docs/ARCHITECTURE.md` — System architecture, component map, data flow
- `docs/BRAIN_SCHEMA.md` — ArangoDB collections, edges, indexes, query patterns
- `docs/TECH_STACK.md` — Rust crates, project structure, Docker Compose
- `docs/EVENT_CATALOG.md` — Event types with payload schemas
- `docs/AGENT_SPEC.md` — Agent YAML format and workflow integration
- `docs/PLUGIN_SPEC.md` — Plugin interface, bootstrap pattern, built-in plugins
- `docs/LLM_INTEGRATION.md` — LLM router, routing strategies, context assembly, tools

## Build Rules

- Rust workspace under `crates/` with `seidrum-common`, `seidrum-kernel`, and `plugins/*`.
- Every event struct must derive `Serialize, Deserialize, Debug, Clone`.
- Use `tracing` for logging, never `println!` in library/service code.
- All brain access from plugins goes through NATS request/reply to kernel.
- Run `cargo build --workspace` after changes to verify compilation.
- Run `cargo clippy --workspace` to catch issues.
- Write unit tests for serialization roundtrips and core logic.

## Conventions

- Module naming: snake_case (e.g., `brain_service`, `plugin_storage`)
- Error handling: use `anyhow::Result` for binaries, `thiserror` for library errors
- Config: environment variables for secrets, YAML files for structure
- No unwrap() in production code — use proper error propagation
- Plugins register on startup via `plugin.register` and deregister on shutdown via `plugin.deregister`
- Capabilities register via `capability.register` with kind `"tool"`, `"command"`, or `"both"`
