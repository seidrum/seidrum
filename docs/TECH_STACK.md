# TECH_STACK.md — Seidrum Technical Stack

## Core Language: Rust

Seidrum is a systems application: a long-running kernel daemon plus independent
plugin processes. Rust for the kernel and core plugins. Plugins can be written
in any language that speaks the bus protocol.

**Why Rust:**
- Single static binary, no runtime, no GC.
- Predictable latency in the event loop.
- Native async with tokio for concurrent event handling.
- Serde for typed JSON serialization of all events.
- ~15MB binaries. ~20MB Docker images from scratch.
- Cross-compile to ARM for Raspberry Pi deployment.

---

## Infrastructure (Docker containers)

Only two infrastructure services:

### seidrum-eventbus
- **Role:** Event backbone. All inter-plugin communication.
- **License:** Apache 2.0
    volumes: ["nats-data:/data"]
    restart: unless-stopped

  arangodb:
    image: arangodb:3.12
    environment:
      ARANGO_ROOT_PASSWORD: ${ARANGO_PASSWORD}
    ports: ["8529:8529"]
    volumes: ["arango-data:/var/lib/arangodb3"]
    restart: unless-stopped

  # --- Kernel ---
  kernel:
    build:
      context: .
      dockerfile: crates/seidrum-kernel/Dockerfile
    command: ["serve"]
    environment:
      BUS_URL: ws://kernel:9000
      ARANGO_URL: http://arangodb:8529
      ARANGO_PASSWORD: ${ARANGO_PASSWORD}
    depends_on: [arangodb]
    restart: unless-stopped

  # --- Plugins ---
  telegram:
    build:
      context: .
      dockerfile: crates/plugins/seidrum-telegram/Dockerfile
    environment:
      BUS_URL: ws://kernel:9000
      TELEGRAM_TOKEN: ${TELEGRAM_TOKEN}
      TELEGRAM_ALLOWED_USERS: ${TELEGRAM_ALLOWED_USERS}
    depends_on: [kernel]
    restart: unless-stopped

  llm-router:
    build:
      context: .
      dockerfile: crates/plugins/seidrum-llm-router/Dockerfile
    environment:
      BUS_URL: ws://kernel:9000
      ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
      OPENAI_API_KEY: ${OPENAI_API_KEY}
      OLLAMA_URL: ${OLLAMA_URL:-}
    depends_on: [kernel]
    restart: unless-stopped

  content-ingester:
    build:
      context: .
      dockerfile: crates/plugins/seidrum-content-ingester/Dockerfile
    environment:
      BUS_URL: ws://kernel:9000
      EMBEDDING_PROVIDER: ${EMBEDDING_PROVIDER:-openai}
      OPENAI_API_KEY: ${OPENAI_API_KEY}
    depends_on: [kernel]
    restart: unless-stopped

  entity-extractor:
    build:
      context: .
      dockerfile: crates/plugins/seidrum-entity-extractor/Dockerfile
    environment:
      BUS_URL: ws://kernel:9000
      ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
    depends_on: [kernel]
    restart: unless-stopped

  fact-extractor:
    build:
      context: .
      dockerfile: crates/plugins/seidrum-fact-extractor/Dockerfile
    environment:
      BUS_URL: ws://kernel:9000
      ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
    depends_on: [kernel]
    restart: unless-stopped

  graph-context-loader:
    build:
      context: .
      dockerfile: crates/plugins/seidrum-graph-context-loader/Dockerfile
    environment:
      BUS_URL: ws://kernel:9000
    depends_on: [kernel]
    restart: unless-stopped

  scope-classifier:
    build:
      context: .
      dockerfile: crates/plugins/seidrum-scope-classifier/Dockerfile
    environment:
      BUS_URL: ws://kernel:9000
      ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
    depends_on: [kernel]
    restart: unless-stopped

  response-formatter:
    build:
      context: .
      dockerfile: crates/plugins/seidrum-response-formatter/Dockerfile
    environment:
      BUS_URL: ws://kernel:9000
    depends_on: [kernel]
    restart: unless-stopped

  event-emitter:
    build:
      context: .
      dockerfile: crates/plugins/seidrum-event-emitter/Dockerfile
    environment:
      BUS_URL: ws://kernel:9000
    depends_on: [kernel]
    restart: unless-stopped

  task-detector:
    build:
      context: .
      dockerfile: crates/plugins/seidrum-task-detector/Dockerfile
    environment:
      BUS_URL: ws://kernel:9000
      ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
    depends_on: [kernel]
    restart: unless-stopped

volumes:
  nats-data:
  arango-data:
```

---

## Dockerfile Pattern (all plugins follow this)

```dockerfile
FROM rust:1.85-slim AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo build --release --package seidrum-telegram \
    --target x86_64-unknown-linux-musl

FROM scratch
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/seidrum-telegram /
ENTRYPOINT ["/seidrum-telegram"]
```

Each plugin image: **~15-20 MB**.

---

## Resource Footprint

| Component            | RAM       | CPU     |
|----------------------|-----------|---------|
| Kernel               | ~30 MB    | minimal |
| EventBus             | ~10 MB    | embedded in kernel |
| ArangoDB             | ~2-4 GB   | 1 core  |
| All plugins combined | ~200 MB   | minimal |
| **Total**            | **~2.5-4.5 GB** | **2 cores** |

Runs on a 4GB VPS. ArangoDB is the only memory-hungry component.

---

## Dependencies Summary

| Component       | Technology       | License     | Image Size |
|-----------------|------------------|-------------|------------|
| Kernel          | Rust             | Your own    | ~15 MB     |
| Each plugin     | Rust             | Your own    | ~15 MB     |
| Event bus       | seidrum-eventbus   | Apache 2.0  | ~20 MB     |
| Knowledge graph | ArangoDB 3.12    | Apache 2.0  | ~400 MB    |
| Telegram        | teloxide (in plugin) | MIT     | compiled in|

**No Python. No Redis. No LiteLLM.** All Rust. All self-hosted.
Only external dependency: LLM provider API keys (user brings their own).
Optionally Ollama for fully local LLM inference.
