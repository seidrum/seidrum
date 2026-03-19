# Seidrum — Build Instructions

## What This Is

Seidrum is an event-driven personal AI agent platform. Rust kernel + NATS JetStream + ArangoDB knowledge graph + plugin architecture.

## Specifications

All specs live in `uploads/seidrum-specs/`. Read the relevant spec before implementing any task:

- `PROJECT.md` — Vision, principles, glossary
- `ARCHITECTURE.md` — System architecture, component map, data flow
- `BRAIN_SCHEMA.md` — ArangoDB collections, edges, indexes, query patterns
- `TECH_STACK.md` — Rust crates, project structure, Docker Compose
- `EVENT_CATALOG.md` — Every event type with full payload schemas (Rust structs)
- `AGENT_SPEC.md` — Agent YAML format, pipeline semantics, prompt templates
- `PLUGIN_SPEC.md` — Plugin interface, bootstrap pattern, all built-in plugins
- `LLM_INTEGRATION.md` — LLM router, routing strategies, context assembly, tools
- `BUILD_PLAN.md` — Ordered task list with dependencies and tests

## Build Rules

- Follow the specs exactly. Event types, collection names, struct fields — all defined.
- Rust workspace under `crates/` with `seidrum-common`, `seidrum-kernel`, and `plugins/*`.
- Use the exact crate dependencies from TECH_STACK.md.
- Every event struct must derive `Serialize, Deserialize, Debug, Clone`.
- Use `tracing` for logging, never `println!` in library/service code.
- All brain access from plugins goes through NATS request/reply to kernel.
- Run `cargo build --workspace` after changes to verify compilation.
- Run `cargo clippy --workspace` to catch issues.
- Write unit tests for serialization roundtrips and core logic.

## Conventions

- Module naming: snake_case matching the spec names (e.g., `brain_service`, `event_registry`)
- Error handling: use `anyhow::Result` for binaries, `thiserror` for library errors
- Config: environment variables for secrets, YAML files for structure
- No unwrap() in production code — use proper error propagation
