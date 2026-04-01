# ARCHITECTURE.md — Seidrum System Architecture

## Core Principle

Seidrum is an event-driven agentic platform. The data flow emerges from
plugin subscriptions — not from a central orchestrator dictating order.

There are 5 concepts:

1. **Events** — typed NATS messages. The universal communication primitive.
2. **Plugins** — independent processes that consume and produce typed events.
3. **Capabilities** — tools, commands, or other invocable functions registered by plugins.
4. **Workflows** — YAML-defined custom wiring that connects plugins, defines agents, and adds routing rules.
5. **Agents** — a prompt + tools + scope configuration used when calling the LLM.

---

## Layer 1: Events (Typed)

Events have two dimensions:

- **Subject** (NATS routing): `channel.telegram.inbound` — determines delivery.
- **Type** (contract): `ChannelInbound` — determines structure.

Multiple subjects can carry the same type:

```
channel.telegram.inbound  → type: ChannelInbound
channel.email.inbound     → type: ChannelInbound
channel.cli.inbound       → type: ChannelInbound
```

Event types are registered in the kernel. The kernel can validate that
a plugin's declared output type matches the next plugin's declared input type.

All events flow through the `EventEnvelope` wrapper:

```rust
struct EventEnvelope {
    id: String,
    event_type: String,        // the type name
    timestamp: DateTime<Utc>,
    source: String,            // plugin that emitted this
    correlation_id: Option<String>,
    scope: Option<String>,
    payload: serde_json::Value,
}
```

---

## Layer 2: Plugins

A plugin is an independent process that:

1. Connects to NATS
2. Registers itself: declares what event **types** it consumes and produces
3. Subscribes to its declared NATS subjects
4. Processes events and publishes results
5. Optionally registers capabilities (tools, commands)
6. Responds to health checks
7. Has its own optional config YAML

**Plugins self-wire.** If plugin A produces events of type `ContentStored`
and plugin B consumes events of type `ContentStored`, they are connected.
No configuration needed. The kernel can generate the full data flow graph
at any time by reading the registry.

### Plugin Categories

All plugins follow the same interface. These categories are conventions,
not architectural distinctions:

| Category | Examples | Pattern |
|----------|----------|---------|
| Channel | telegram, cli, email, calendar | Bidirectional: produces `channel.*.inbound`, consumes `channel.*.outbound` |
| LLM Provider | llm-google, llm-openai, llm-ollama | Consumes `llm.provider.{id}`, produces reply via NATS request/reply |
| Processing | content-ingester, entity-extractor | Consumes one event type, produces another |
| Infrastructure | llm-router, tool-dispatcher, response-formatter | Orchestration and routing |

### LLM Router

The LLM Router is a plugin that:

- Consumes: `channel.*.inbound` and `agent.context.loaded`
- Produces: `llm.response`
- Assembles context (prompt template, token budgeting)
- Queries the capability registry for available tools
- Dispatches to an LLM provider via `llm.provider.{id}` (NATS request/reply)
- Receives the provider's response and publishes `llm.response`

The router speaks **unified format only**. It never touches provider-specific
JSON. Its config YAML specifies: default provider, model, fallback chain,
temperature, max tokens.

### LLM Providers

LLM Provider plugins are adapters:

- Consume: `llm.provider.{id}` (unified format)
- Produce: reply via NATS request/reply (unified format)
- Internally: translate unified → provider API format, call HTTP API
- Handle the **tool call loop** internally:
  - If the LLM returns a function call → translate to unified `capability.call`
  - Dispatch via NATS to the capability dispatcher
  - Get result, translate back to provider format
  - Call the LLM again, repeat until content
- Only return the final response (all tool calls resolved)

Each provider has its own config YAML (API keys, model defaults, etc.).

### Capability Dispatcher

Routes `capability.call` events to the owning plugin:

- Consumes: `capability.call` (NATS request/reply)
- Looks up which plugin owns the capability
- Forwards to `capability.call.{plugin_id}`
- Returns the result

---

## Layer 3: Capabilities

A unified registry for anything plugins expose as invocable:

```rust
struct CapabilityRegister {
    id: String,              // "execute-code"
    plugin_id: String,       // "code-executor"
    name: String,            // "Execute Code"
    kind: String,            // "tool" | "command" | "both" | future types
    summary_md: String,
    manual_md: String,
    parameters: Value,       // JSON Schema
    call_subject: String,    // "capability.call.code-executor"
    metadata: HashMap<String, Value>,
}
```

- **kind = "tool"**: discoverable by the LLM via the capability registry
- **kind = "command"**: invocable by the user via `/` prefix in channels
- **kind = "both"**: available to both LLM and user
- **kind = anything else**: future extensibility (webhooks, cron triggers, etc.)

The `kind` field is a string, not an enum. Third-party plugins can
introduce new kinds without changing the kernel.

Capabilities are stored in the `capabilities` ArangoDB collection with
full-text search over `summary_md` and `manual_md`.

---

## Layer 4: Workflows

Workflows are YAML files that add custom wiring on top of the natural
plugin connections. They do NOT replace the plugin system — they layer
on top of it.

### What Workflows Can Do

**Positive workflows** — add new behavior:
- Connect plugins that aren't naturally connected
- Define event transformers between incompatible types
- Add routing rules (e.g., "email responses go to Telegram")

**Negative workflows** — restrict existing behavior:
- Block certain event flows (e.g., "never auto-reply to email")
- Gate flows with human approval
- Add conditions to existing connections

### Workflow Structure

```yaml
workflow:
  id: email-triage
  description: Monitor email, classify, draft replies with approval

  # Agents used by this workflow
  agents:
    email-classifier:
      prompt: ./prompts/email-classifier.md
      tools: [brain-query]
      scope: scope_work

    email-responder:
      prompt: ./prompts/email-responder.md
      tools: [brain-query, send-email]
      scope: scope_work

  # Custom wiring rules
  on: channel.email.inbound

  steps:
    - plugin: content-ingester

    - agent: email-classifier

    - condition:
        if: "result.needs_reply == true"
        then: continue
        else: goto notify_only

    - agent: email-responder

    - gate:
        notify:
          channel: telegram
          chat_id: "9287635"
        prompt: "Reply to {{from}}?\n\n{{draft}}"
        actions: [approve, discard]

    - output:
        channel: email
      when: approved

    - id: notify_only
      output:
        channel: telegram
        chat_id: "9287635"
        template: "Email from {{from}}: {{subject}}"
```

### Event Transformers

When two plugins produce/consume incompatible event types, a transformer
bridges the gap:

```yaml
steps:
  - plugin: content-ingester
    # produces: ContentStored

  - transform:
      from: ContentStored
      to: ChannelInbound
      map:
        text: "$input.raw_text"
        platform: "internal"
        user_id: "$input.metadata.from"
        chat_id: "$input.channel_id"

  - plugin: some-plugin-that-expects-ChannelInbound
```

Transformers can be:
- **Inline**: field mappings defined in the YAML
- **Plugin**: a transformer plugin that does complex conversion

### Workflow Storage

Workflows are authored in YAML (source of truth). On `seidrum init` or
`seidrum load-workflows`, they are parsed and stored in ArangoDB as
graph structures (nodes + edges) for runtime traversal and querying.

---

## Layer 5: Agents

An agent is NOT a plugin. An agent is a **configuration**:

```yaml
agent:
  id: assistant
  prompt: ./prompts/assistant.md
  tools: [brain-query, execute-code, search-calendar]
  scope: scope_root
```

- **prompt**: path to a Tera template file
- **tools**: list of capability IDs available to the LLM
- **scope**: brain knowledge boundary for this agent

When a workflow step says `agent: assistant`, it means:
"Take the current message, apply this prompt and these tools,
send to the LLM router." The LLM router uses the agent config
to assemble the context and select tools.

Agents are defined in their own YAML files under `agents/` and
referenced by ID in workflows.

---

## Data Flow: Emergent Graph

The kernel can generate the complete data flow graph at any time:

```
FOR plugin IN plugins
  FOR consumed_type IN plugin.consumes
    FOR producer IN plugins
      FILTER consumed_type IN producer.produces
      RETURN { from: producer.id, to: plugin.id, event_type: consumed_type }
```

This graph is the **ground truth** of how data flows. Workflows add
edges (positive) or remove/gate edges (negative) on top of this graph.

Example emergent graph (no workflow needed):

```
telegram ──channel.*.inbound──▶ content-ingester
telegram ──channel.*.inbound──▶ graph-context-loader
telegram ──channel.*.inbound──▶ llm-router
llm-router ──llm.provider.google──▶ llm-google
llm-google ──capability.call──▶ capability-dispatcher
llm-router ──llm.response──▶ response-formatter
response-formatter ──channel.telegram.outbound──▶ telegram
```

This works without any workflow definition. Plugins self-wire.

---

## Response Routing

The orchestrator handles response routing:

1. When a trigger event arrives from a channel, the orchestrator stores:
   `correlation_id → { platform, chat_id, thread_id, message_id }`

2. The pipeline runs. Plugins know nothing about channels.

3. When `llm.response` arrives, the orchestrator:
   - Looks up the correlation_id
   - Knows the origin channel
   - Routes to the correct `channel.{platform}.outbound`

4. If a workflow overrides the response channel, the orchestrator
   routes to the workflow-defined output instead.

Default: respond on the same channel. Override: workflow config.

---

## Security Layer

Authentication, authorization, rate limiting, and audit logging are
enforced at the API Gateway — the single entry point for all external
access. See [SECURITY.md](SECURITY.md) for full details.

### Auth Middleware in Request Flow

Every authenticated HTTP request passes through the `auth_rate_limit_middleware`
before reaching any handler:

```
External Request
  │
  ├─ /api/v1/health ─────────────────────▶ Handler (no auth)
  ├─ /dashboard/* ────────────────────────▶ Static files (no auth)
  │
  └─ /api/v1/* ──┬─ 1. Authenticate ─────▶ AuthHandler
                 │     (JWT Bearer or API key — constant-time comparison)
                 │
                 ├─ 2. Rate limit ────────▶ RateLimiter (token bucket)
                 │     (per-subject, role-aware RPM limits)
                 │
                 ├─ 3. Handler ───────────▶ Route handler
                 │     (auth result available via request extensions)
                 │
                 └─ 4. Audit ─────────────▶ AuditLog
                       (in-memory ring buffer + ArangoDB via NATS)
```

Failures at any stage short-circuit the pipeline:
- Auth failure → `401 Unauthorized` + audit entry
- Rate limit exceeded → `429 Too Many Requests` + `Retry-After` header + audit entry

### Multi-User Data Isolation

Multi-user support introduces user-scoped data boundaries:

1. **User records** are stored in the `users` ArangoDB collection with
   Argon2id password hashes.
2. **JWT tokens** carry `user_id` and `scopes` claims that the kernel
   uses to filter brain queries.
3. **Scope enforcement** is extended: the `scoped_to` edge collection
   now connects to both `scopes` and `users` vertices, enabling
   per-user knowledge isolation.
4. **Audit trails** include `user_id` on every entry for accountability.
5. **User API keys** in the `api_keys` collection provide programmatic
   access scoped to a specific user.

### Audit Pipeline

```
API Gateway (auth event occurs)
  │
  ├─ In-memory ring buffer (1,000 entries, fast dashboard queries)
  │
  └─ NATS publish "brain.audit.store" (fire-and-forget)
       └─ Kernel Brain Service
            └─ ArangoDB "audit_log" collection (permanent storage)
```

---

## What the Kernel Owns

1. **Brain** (ArangoDB) — knowledge graph, capabilities collection, users, audit log, API keys
2. **Plugin Registry** — tracks running plugins, their consumed/produced types
3. **Capability Registry** — tools, commands, and future capability kinds
4. **Workflow Engine** — loads workflows, applies wiring rules, manages routing
5. **Scheduler** — cron jobs (fact decay, health monitoring)
6. **Scope Enforcement** — filters brain queries by scope boundaries
7. **User Management** — CRUD for user accounts (`brain.user.*` subjects)
8. **Audit Persistence** — stores and queries audit entries (`brain.audit.*` subjects)
9. **API Key Management** — CRUD for user-scoped API keys (`brain.apikey.*` subjects)

---

## Plugin Config

Each plugin has its own optional config YAML:

```
config/
  platform.yaml           # kernel config (NATS URL, ArangoDB, etc.)
  llm-router.yaml         # provider, model, fallback, temperature
  llm-google.yaml         # API key, model defaults
  telegram.yaml           # token, allowed users, whisper paths
  email.yaml              # IMAP/SMTP credentials
```

Plugin config is separate from workflow definitions. A plugin's config
determines HOW it operates. A workflow determines WHEN and WHERE it
operates in the data flow.

---

## Summary

| Concept | What it is | Who defines it |
|---------|-----------|----------------|
| Event | Typed NATS message | Plugins emit/consume |
| Plugin | Code that runs, self-registers | Developer |
| Capability | Registered tool/command/future | Plugins register |
| Data flow graph | Emerges from plugin registrations | Automatic |
| Workflow | Custom wiring + agents + routing rules | User (YAML) |
| Agent | Prompt + tools + scope | User (YAML, referenced by workflows) |
| Plugin config | How a plugin operates | User (YAML per plugin) |
