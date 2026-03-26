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

### Plugins (20)

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
| `seidrum-api-gateway` | Infra | WebSocket + REST API for external plugins in any language |
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
- [Docker](https://docker.com/get-started) (for ArangoDB)

### Quick start

```bash
git clone https://github.com/seidrum/seidrum.git
cd seidrum
cargo build --workspace --release

# Interactive setup — downloads NATS, pulls ArangoDB, configures API keys
seidrum setup

# Start everything (NATS + ArangoDB + kernel + plugins)
seidrum start
```

Dashboard: http://localhost:8080/dashboard

See [docs/GETTING_STARTED.md](docs/GETTING_STARTED.md) for detailed instructions.

### Using the `seidrum` CLI

```bash
# Managed mode (handles infrastructure automatically)
seidrum setup                 # First-run wizard: downloads NATS, configures everything
seidrum start                 # Start infrastructure + kernel + all enabled plugins
seidrum stop                  # Stop everything
seidrum status                # Show infrastructure + process status

# Power-user mode (you manage NATS/ArangoDB yourself)
seidrum daemon start          # Start kernel + enabled plugins only (foreground)
seidrum daemon stop           # Graceful shutdown
seidrum daemon status         # Show process status

# Install as system service (systemd on Linux, launchd on macOS)
seidrum daemon install        # Install, enable, and start the service
seidrum daemon uninstall      # Stop and remove the service

# Plugin management
seidrum plugin list           # Show all plugins with enabled/disabled state
seidrum plugin enable telegram
seidrum plugin disable email
seidrum plugin start claude-code
seidrum plugin stop claude-code

# Database and config
seidrum init                  # Initialize ArangoDB collections
seidrum validate              # Validate platform and agent configuration
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
| `config/plugins.yaml` | Plugin manifest — binaries, enabled state, env vars |
| `prompts/*.md` | Tera-templated system prompts |

## Project structure

```
seidrum/
├── crates/
│   ├── seidrum-common/        # Shared types, events, NATS utilities
│   ├── seidrum-kernel/        # Core services (brain, registry, scheduler)
│   ├── seidrum-daemon/        # Unified CLI + process supervisor (`seidrum` binary)
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
│       ├── seidrum-calendar/
│       └── seidrum-api-gateway/
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

### Agent consciousness

Agents in Seidrum are not request-response handlers -- they are persistent, autonomous processes. Each agent runs a **consciousness loop** that continuously processes events from a dedicated NATS stream (`agent.{id}.consciousness`). Events include user messages, subscribed system events, self-scheduled wake-ups, and messages from other agents.

**Built-in capabilities** available to every agent: `brain-query`, `subscribe-events`, `unsubscribe-events`, `delegate-task`, `schedule-wake`, `send-notification`, `get-conversation`, `list-conversations`, `search-skills`, `load-skill`, `save-skill`.

**Conversations** are first-class objects stored in the brain. Agents maintain structured conversation threads across platforms, preserving tool calls, media attachments, and inner monologue for continuity across sessions.

**Skills** provide behavioral RAG -- reusable instructions and procedures retrieved by semantic similarity at inference time. Skills come from YAML files (`skills/` directory), user instructions, agent self-learning, or packaged distributions installed via `seidrum skill install`. See [Agent Consciousness](docs/AGENT_CONSCIOUSNESS.md) and [Agent Skills](docs/AGENT_SKILLS.md) for details.

Plugins can also be written in **any language** using the API gateway. The gateway exposes a WebSocket and REST API that bridges to NATS:

```bash
# Start the API gateway
GATEWAY_API_KEY=my-secret target/debug/seidrum-api-gateway
```

**WebSocket** (`ws://localhost:8080/ws?api_key=my-secret`):

```json
{"type": "register", "plugin": {"id": "my-plugin", "name": "My Plugin", "version": "0.1.0", "description": "Does things"}}
{"type": "register_capability", "capability": {"tool_id": "my-tool", "plugin_id": "my-plugin", "name": "My Tool", "summary_md": "Does something", "manual_md": "...", "parameters": {}, "call_subject": "capability.call.my-plugin", "kind": "tool"}}
{"type": "subscribe", "subjects": ["channel.*.inbound"]}
```

**REST** (`Authorization: Bearer my-secret`):

```bash
# Call a capability
curl -X POST http://localhost:8080/api/v1/capabilities/execute-code/call \
  -H "Authorization: Bearer my-secret" \
  -d '{"arguments": {"language": "python", "code": "print(42)"}}'

# Storage operations
curl -X POST http://localhost:8080/api/v1/storage/set \
  -H "Authorization: Bearer my-secret" \
  -d '{"plugin_id": "my-plugin", "key": "count", "value": 42}'
```

## Documentation

Detailed design documents are in [`docs/`](docs/):

- [Project vision and principles](docs/PROJECT.md)
- [System architecture](docs/ARCHITECTURE.md)
- [Brain schema (ArangoDB)](docs/BRAIN_SCHEMA.md)
- [Plugin specification](docs/PLUGIN_SPEC.md)
- [Event catalog](docs/EVENT_CATALOG.md)
- [LLM integration](docs/LLM_INTEGRATION.md)
- [Agent specification](docs/AGENT_SPEC.md)
- [Agent consciousness](docs/AGENT_CONSCIOUSNESS.md)
- [Agent skills](docs/AGENT_SKILLS.md)
- [Tech stack](docs/TECH_STACK.md)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

[MIT](LICENSE)
