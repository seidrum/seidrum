# Seidrum

An event-driven personal AI agent platform. Rust kernel, NATS JetStream messaging, ArangoDB knowledge graph, plugin architecture.

From Old Norse *seidr* (seeing hidden connections) + *rum* (space). Pronounced **SAY-drum**.

## What is Seidrum?

Seidrum connects LLMs to your digital life through a persistent knowledge graph. Messages, emails, files, and calendar events flow through a plugin pipeline that extracts entities, builds relationships, and maintains temporal facts with confidence scores.

There is no architectural distinction between a Telegram adapter, an LLM provider, or an entity extractor. They are all **plugins** — independent processes that consume and produce typed NATS events. The kernel is minimal: it owns the brain (ArangoDB), the plugin registry, and the workflow engine. Everything else is a plugin.

### Key ideas

- **Events are the universal primitive.** Every interaction is a typed NATS message. The system routes events, not function calls.
- **Everything is a plugin.** Telegram, LLM providers, entity extraction, code execution — all independent processes speaking NATS.
- **The brain is a graph, not a log.** Entities, facts, and relationships form a knowledge graph. Facts are temporal — they have confidence, provenance, and decay over time.
- **Scopes are boundaries.** An agent in the "job search" scope cannot see "personal finance" data unless explicitly granted access.
- **Always-on kernel.** Plugins register and deregister dynamically. No kernel restart needed when adding new capabilities.

## Architecture

```
                    ┌─────────────────────────────┐
                    │         Kernel (Rust)        │
                    │  ┌───────┐ ┌──────────────┐  │
                    │  │ Brain │ │   Registries  │  │
                    │  │(Arango│ │ Plugin | Cap  │  │
                    │  │  DB)  │ │ Storage| Sched│  │
                    │  └───────┘ └──────────────┘  │
                    │      Workflow Engine          │
                    └──────────┬──────────────────┘
                               │ NATS JetStream
        ┌──────────┬───────────┼───────────┬──────────┐
        │          │           │           │          │
   ┌────┴───┐ ┌────┴────┐ ┌───┴───┐ ┌─────┴────┐ ┌───┴────┐
   │Telegram│ │LLM Router│ │Content│ │  Tool    │ │Response│
   │        │ │+ Provider│ │Ingest │ │Dispatcher│ │Formattr│
   └────────┘ └─────────┘ └───────┘ └──────────┘ └────────┘
        │          │           │           │          │
   ┌────┴───┐ ┌────┴────┐ ┌───┴───┐ ┌─────┴────┐ ┌───┴────┐
   │  CLI   │ │  Claude  │ │Entity │ │  Code    │ │ Event  │
   │        │ │  Code    │ │Extract│ │ Executor │ │Emitter │
   └────────┘ └─────────┘ └───────┘ └──────────┘ └────────┘
```

### Kernel services

| Service | Purpose |
|---------|---------|
| Brain | ArangoDB knowledge graph — entities, content, facts, scopes, tasks |
| Plugin Registry | Tracks running plugins, consumed/produced event types |
| Capability Registry | Tools, commands, and capabilities registered by plugins |
| Plugin Storage | Persistent key-value store for plugin state |
| Workflow Engine | Loads workflow YAML, wires plugins, manages routing |
| Scheduler | Cron jobs — fact confidence decay, health monitoring, auto-cleanup |

### Plugins (19)

| Plugin | Type | Description |
|--------|------|-------------|
| `seidrum-telegram` | Channel | Telegram Bot API — text, voice, images, commands |
| `seidrum-cli` | Channel | Terminal stdin/stdout interface |
| `seidrum-email` | Channel | IMAP/SMTP email integration |
| `seidrum-calendar` | Channel | Google Calendar integration |
| `seidrum-llm-router` | LLM | Routes requests to providers, assembles context, manages tool loops |
| `seidrum-llm-google` | LLM | Google Gemini provider adapter |
| `seidrum-claude-code` | Tool | Claude Code CLI — agentic coding tasks |
| `seidrum-code-executor` | Tool | Sandboxed Python/Bash/JS execution |
| `seidrum-tool-dispatcher` | Infra | Routes capability calls to owning plugins |
| `seidrum-content-ingester` | Processing | Ingests messages into the knowledge graph |
| `seidrum-entity-extractor` | Processing | Extracts entities (people, orgs, tools) from content |
| `seidrum-fact-extractor` | Processing | Extracts temporal facts with confidence scores |
| `seidrum-graph-context-loader` | Processing | Loads relevant graph context for LLM prompts |
| `seidrum-scope-classifier` | Processing | Classifies content into scopes |
| `seidrum-task-detector` | Processing | Detects actionable tasks from conversations |
| `seidrum-response-formatter` | Infra | Formats LLM responses per channel (markdown, plain) |
| `seidrum-event-emitter` | Infra | Extracts structured actions from LLM responses |
| `seidrum-notification` | Infra | Routes notifications to channels |

## Getting started

### Prerequisites

- [Rust](https://rustup.rs/) (stable)
- [NATS Server](https://nats.io/) with JetStream enabled
- [ArangoDB](https://arangodb.com/) 3.12+
- (Optional) [Docker Compose](https://docs.docker.com/compose/) for infrastructure

### Quick start

```bash
# Clone
git clone https://github.com/seidrum/seidrum.git
cd seidrum

# Start infrastructure
docker compose up -d nats arangodb

# Configure
cp .env.example .env
# Edit .env with your API keys and tokens

# Build
cargo build --workspace

# Initialize the brain database
source .env && target/debug/seidrum-kernel init

# Start the kernel
source .env && target/debug/seidrum-kernel serve &

# Start plugins (example: telegram + llm)
source .env && target/debug/seidrum-telegram &
source .env && target/debug/seidrum-llm-router &
source .env && target/debug/seidrum-llm-google &
source .env && target/debug/seidrum-tool-dispatcher &
source .env && target/debug/seidrum-response-formatter &
```

### Docker Compose (full stack)

```bash
cp .env.example .env
# Edit .env with your API keys and tokens
docker compose up -d
```

### Configuration

| File | Purpose |
|------|---------|
| `.env` | Secrets — API keys, tokens, passwords |
| `config/platform.yaml` | Kernel config — NATS URL, ArangoDB connection |
| `agents/*.yaml` | Agent definitions — prompt, tools, scope |
| `workflows/*.yaml` | Workflow wiring — triggers, steps, routing |
| `prompts/*.md` | Tera-templated system prompts |

## Project structure

```
seidrum/
├── crates/
│   ├── seidrum-common/        # Shared types, events, NATS utilities
│   ├── seidrum-kernel/        # Core services (brain, registry, scheduler)
│   └── plugins/
│       ├── seidrum-telegram/
│       ├── seidrum-cli/
│       ├── seidrum-llm-router/
│       ├── seidrum-llm-google/
│       ├── seidrum-claude-code/
│       ├── seidrum-code-executor/
│       ├── seidrum-tool-dispatcher/
│       ├── seidrum-content-ingester/
│       ├── seidrum-entity-extractor/
│       ├── seidrum-fact-extractor/
│       ├── seidrum-graph-context-loader/
│       ├── seidrum-scope-classifier/
│       ├── seidrum-task-detector/
│       ├── seidrum-response-formatter/
│       ├── seidrum-event-emitter/
│       ├── seidrum-notification/
│       ├── seidrum-email/
│       └── seidrum-calendar/
├── agents/                    # Agent YAML definitions
├── workflows/                 # Workflow YAML definitions
├── prompts/                   # Tera-templated system prompts
├── config/                    # Platform configuration
├── docs/                      # Design documents and specs
└── docker-compose.yml
```

## Writing a plugin

A plugin is any process that connects to NATS, registers itself, and processes events. Here's the minimal pattern in Rust:

```rust
use seidrum_common::events::PluginRegister;
use seidrum_common::nats_utils::NatsClient;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let nats = NatsClient::connect("nats://localhost:4222", "my-plugin").await?;

    // Register with the kernel
    let register = PluginRegister {
        id: "my-plugin".to_string(),
        name: "My Plugin".to_string(),
        version: "0.1.0".to_string(),
        description: "Does something useful".to_string(),
        consumes: vec!["channel.*.inbound".to_string()],
        produces: vec!["my.plugin.output".to_string()],
        health_subject: "plugin.my-plugin.health".to_string(),
        consumed_event_types: vec![],
        produced_event_types: vec![],
    };
    nats.publish_envelope("plugin.register", None, None, &register).await?;

    // Subscribe and process events
    let mut sub = nats.subscribe("channel.*.inbound").await?;
    while let Some(msg) = sub.next().await {
        // Process the event...
    }
    Ok(())
}
```

Plugins can be written in any language that speaks NATS.

## Documentation

Detailed design documents are in [`docs/`](docs/):

- [Project vision and principles](docs/PROJECT.md)
- [System architecture](docs/ARCHITECTURE.md)
- [Brain schema (ArangoDB)](docs/BRAIN_SCHEMA.md)
- [Plugin specification](docs/PLUGIN_SPEC.md)
- [Event catalog](docs/EVENT_CATALOG.md)
- [LLM integration](docs/LLM_INTEGRATION.md)
- [Agent specification](docs/AGENT_SPEC.md)
- [Tech stack](docs/TECH_STACK.md)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

[MIT](LICENSE)
