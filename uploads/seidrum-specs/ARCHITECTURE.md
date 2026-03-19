# ARCHITECTURE.md — Seidrum System Architecture

## Core Insight

There is no distinction between channel adapters, LLM clients, ingestion
processors, or middleware. They are all **plugins**: independent processes
that connect to NATS, subscribe to event types they consume, and publish
event types they produce.

The kernel is minimal. It owns the brain, the event type registry, and the
agent orchestrator. Everything else lives outside.

## What the Kernel Owns

### 1. Brain (ArangoDB)

The knowledge graph. Only the kernel has direct ArangoDB access. Plugins
communicate with the brain through NATS request/reply events:

- `brain.query.request` → `brain.query.response` (read)
- `brain.content.store` → `brain.content.stored` (write)
- `brain.entity.upsert` → `brain.entity.upserted` (write)
- `brain.fact.upsert` → `brain.fact.upserted` (write)
- `brain.scope.assign` → `brain.scope.assigned` (write)
- `brain.task.upsert` → `brain.task.upserted` (write)

Plugins never need an ArangoDB connection. They ask the kernel via NATS,
the kernel enforces scopes, and returns results.

Brain access pattern:

```
Plugin                          Kernel
  │                               │
  │  brain.query.request          │
  │  { aql, params, scope }      │
  │──────────────────────────────▶│
  │                               │ ← enforces scope
  │                               │ ← executes AQL
  │  brain.query.response         │
  │  { results, count }          │
  │◀──────────────────────────────│
```

NATS supports request/reply natively — the plugin publishes a request and
awaits a response on an auto-generated inbox subject.

This is critical because:
1. **Scope enforcement is centralized.** The kernel injects scope filters
   into every query. No plugin can bypass scoping.
2. **Single connection.** Only one process holds the ArangoDB connection pool.
3. **Auditability.** Every brain access is a NATS event that can be logged.

### 2. Event Type Registry

Every event type that flows through the system must be registered:

```rust
struct EventTypeRegistration {
    /// NATS subject pattern (e.g., "channel.telegram.inbound")
    subject: String,

    /// JSON Schema for the payload
    schema: serde_json::Value,

    /// Human-readable description
    description: String,

    /// Which plugin(s) produce this event type
    producers: Vec<String>,

    /// Which plugin(s) consume this event type
    consumers: Vec<String>,
}
```

The registry serves two purposes:
- **Validation:** The kernel can optionally validate events against schemas.
- **Discovery:** Plugins and agents can query what event types exist.

Stored in ArangoDB (a simple collection) and cached in memory.
New plugins register their event types on startup via `plugin.register`.

### 3. Agent Orchestrator

Reads agent YAML definitions. An agent is a declared pipeline — a sequence
of plugins wired together via event types. The orchestrator:

- Validates that the pipeline is coherent (each step's output event type
  matches the next step's input)
- Creates NATS subscription chains to implement the pipeline
- Monitors pipeline health (is each plugin responsive?)
- Injects scope context at appropriate points

## Component Map

```
┌─────────────────────────────────────────────────────────────────┐
│                         NATS JetStream                          │
│         (event backbone — all communication flows here)         │
└───┬────────┬──────────┬───────────┬───────────┬────────────┬────┘
    │        │          │           │           │            │
┌───▼──┐ ┌──▼───┐ ┌────▼────┐ ┌───▼─────┐ ┌──▼──────┐ ┌───▼──────┐
│Kernel│ │Tele- │ │  LLM    │ │ Content │ │ Entity  │ │  Graph   │
│      │ │gram  │ │ Router  │ │Ingester │ │Extractor│ │ Context  │
│Brain │ │Plugin│ │ Plugin  │ │ Plugin  │ │ Plugin  │ │ Loader   │
│Reg.  │ │      │ │         │ │         │ │         │ │ Plugin   │
│Orch. │ │bi-dir│ │consumes:│ │consumes:│ │consumes:│ │consumes: │
│      │ │      │ │llm.req.*│ │chan.*.in│ │brain.   │ │chan.*.in │
│      │ │      │ │produces:│ │produces:│ │content. │ │produces: │
│      │ │      │ │llm.resp │ │brain.   │ │stored   │ │agent.    │
│      │ │      │ │         │ │content. │ │produces:│ │context.  │
│      │ │      │ │         │ │store    │ │brain.   │ │loaded    │
│      │ │      │ │         │ │         │ │entity.  │ │          │
│      │ │      │ │         │ │         │ │upsert   │ │          │
└──────┘ └──────┘ └─────────┘ └─────────┘ └─────────┘ └──────────┘

    ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────┐
    │  Fact    │ │  Scope   │ │ Response │ │  Event   │
    │Extractor │ │Classifier│ │Formatter │ │ Emitter  │
    │ Plugin   │ │ Plugin   │ │ Plugin   │ │ Plugin   │
    │          │ │          │ │          │ │          │
    │consumes: │ │consumes: │ │consumes: │ │consumes: │
    │brain.    │ │brain.    │ │llm.resp  │ │llm.resp  │
    │entity.   │ │content.  │ │produces: │ │produces: │
    │upserted  │ │stored    │ │chan.*.   │ │task.     │
    │produces: │ │produces: │ │outbound  │ │created   │
    │brain.    │ │brain.    │ │          │ │brain.    │
    │fact.     │ │scope.    │ │          │ │fact.     │
    │upsert    │ │assign    │ │          │ │upsert    │
    └──────────┘ └──────────┘ └──────────┘ └──────────┘
```

Every box except the kernel is an independent process. They only communicate
through NATS. They can be written in any language. They can be restarted
independently. They can run on different machines.

## Data Flow: User Message → Agent Response

```
1. User sends "What's the status of my Infrahub interview?" via Telegram

2. Telegram Plugin receives message via Bot API
   → Publishes to NATS: channel.telegram.inbound
   → Payload: { text, user_id, chat_id, timestamp }

3. Kernel's Agent Orchestrator sees channel.telegram.inbound
   → Matches against personal-assistant agent pipeline triggers
   → Pipeline begins

4. Content Ingester Plugin (consumes channel.telegram.inbound):
   → Publishes brain.content.store to kernel
   → Kernel stores content in ArangoDB, generates embedding
   → Kernel publishes brain.content.stored
   → Ingester passes original event through

5. Graph Context Loader Plugin (consumes channel.telegram.inbound):
   → Publishes brain.query.request to kernel:
     "vector search for similar content + expand graph neighborhood"
   → Kernel enforces scope, executes query, returns results
   → Attaches context (entities, facts, similar content) to event
   → Publishes agent.context.loaded

6. LLM Router Plugin (consumes agent.context.loaded):
   → Assembles context window: prompt template + facts + history + tools
   → Selects model based on routing strategy
   → Calls LLM provider API (Anthropic, OpenAI, Ollama, etc.)
   → Handles tool calls if needed (loop: call → tool → call)
   → Publishes llm.response

7. Event Emitter Plugin (consumes llm.response):
   → Parses LLM output for structured actions
   → If task detected → publishes task.created
   → If fact detected → publishes brain.fact.upsert
   → Passes response through

8. Response Formatter Plugin (consumes llm.response):
   → Formats for Telegram (markdown, buttons)
   → Publishes channel.telegram.outbound

9. Telegram Plugin (consumes channel.telegram.outbound):
   → Sends formatted message via Telegram Bot API

10. BACKGROUND (async, doesn't block response):
    → brain.content.stored triggers Entity Extractor Plugin
    → brain.entity.upserted triggers Fact Extractor Plugin
    → brain.content.stored triggers Scope Classifier Plugin
```

## Agent as a Wiring Diagram

An agent is a declared flow of event types through plugins:

```yaml
# agents/personal-assistant.yaml
agent:
  id: personal-assistant
  name: Personal Assistant
  scope: scope_root
  additional_scopes: [scope_job_search, scope_projects]

  pipeline:
    triggers:
      - channel.telegram.inbound
      - channel.cli.inbound
      - task.completed.*

    steps:
      - plugin: content-ingester
        consumes: trigger
        produces: brain.content.stored

      - plugin: graph-context-loader
        consumes: trigger
        produces: agent.context.loaded
        config:
          graph_depth: 3
          max_facts: 50

      - plugin: llm-router
        consumes: agent.context.loaded
        produces: llm.response
        config:
          strategy: best-first
          models: [claude-sonnet-4-20250514, gpt-4o]
          prompt: ./prompts/assistant.md
          tools:
            registry_access: true
            pinned: [brain-query, web-search]

      - plugin: event-emitter
        consumes: llm.response
        produces: [task.created, brain.fact.upsert]

      - plugin: response-formatter
        consumes: llm.response
        produces: channel.*.outbound

  background:
    - plugin: entity-extractor
      consumes: brain.content.stored
      produces: brain.entity.upsert

    - plugin: fact-extractor
      consumes: brain.entity.upserted
      produces: brain.fact.upsert

    - plugin: scope-classifier
      consumes: brain.content.stored
      produces: brain.scope.assign
```

The orchestrator validates the chain at startup: does each step's output
match the next step's input? Are all referenced plugins registered?

## Plugin Lifecycle

```
1. START     — Plugin process starts, connects to NATS
2. REGISTER  — Publishes plugin.register with its declaration
               Kernel validates and stores in registry
3. READY     — Plugin subscribes to its declared event types
4. HEALTHY   — Plugin responds to periodic health pings on plugin.{id}.health
5. PROCESS   — Plugin receives events, publishes events
6. SHUTDOWN  — Plugin publishes plugin.{id}.shutdown, unsubscribes, exits
```

## Concurrency Model

The kernel runs on a single tokio runtime:

```
tokio::main
├── nats_connection (1 task)
├── brain_service (1 task)
│   └── handles brain.*.request subjects
├── registry_service (1 task)
│   └── handles plugin.register and registry queries
├── agent_orchestrator (1 task)
│   └── manages pipeline wiring and health monitoring
├── scheduler (1 task)
│   └── cron: fact decay, health checks, compaction
└── brain_connection_pool
    └── reqwest client with connection pooling to ArangoDB
```

Plugins run as separate OS processes (Docker containers or bare binaries).
They each have their own async runtime and NATS connection.

## Error Handling Strategy

- **NATS disconnection:** async-nats auto-reconnects. JetStream buffers
  undelivered events for replay on reconnect.
- **ArangoDB down:** Brain requests return error responses. Plugins that need
  brain data degrade gracefully. Events are not lost (NATS persistence).
- **Plugin crash:** Events queue in NATS JetStream. When plugin restarts,
  it consumes queued events. Other plugins unaffected.
- **LLM provider down:** LLM router plugin implements fallback chains.
  Tries next provider. Returns error event if all fail.
- **Invalid config:** Caught at startup by `seidrum validate`. The kernel
  refuses to start with invalid YAML or broken pipeline chains.

## Security Model (v1)

- **Single user, single machine.** No authentication layer.
- **NATS:** No TLS for v1 (localhost / Docker network traffic only).
- **ArangoDB:** Password auth via environment variable. Only kernel connects.
- **Telegram plugin:** Bot token in env. User ID whitelist.
- **Scope enforcement:** All brain queries pass through the kernel which
  injects scope filters. No plugin has direct ArangoDB access.
- **Plugin isolation:** Each plugin is a separate process. A crashed or
  malicious plugin cannot affect the kernel or other plugins beyond
  publishing invalid events (which the registry can optionally validate).
