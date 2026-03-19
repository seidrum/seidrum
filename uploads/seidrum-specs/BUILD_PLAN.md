# BUILD_PLAN.md — Seidrum Ordered Build Tasks

## How to Use This Document

Each phase has numbered tasks. Complete in order. Each task specifies:
- **What to build** (concrete deliverable)
- **Depends on** (which tasks must be done first)
- **Test** (how to verify it works)
- **Reference** (which spec document has details)

For each task, feed Claude Code: BUILD_PLAN.md + PROJECT.md + referenced specs.

---

## Phase 1: Kernel Skeleton

### Task 1.1: Cargo Workspace

**What:** Initialize Rust workspace. Create crate structure:
- `seidrum-common` (shared lib: event types, NATS helpers, config types)
- `seidrum-kernel` (binary: brain, registry, orchestrator)
- `plugins/seidrum-cli` (binary: CLI channel plugin)
Set up clap CLI for kernel: `seidrum serve`, `seidrum init`, `seidrum validate`.

**Depends on:** Nothing.
**Test:** `cargo build --workspace` succeeds.
**Reference:** TECH_STACK.md (Project Structure, Crate Dependencies)

---

### Task 1.2: Event Types (seidrum-common)

**What:** Define all event types as Rust structs with serde. EventEnvelope
wrapper. Every type from EVENT_CATALOG.md as a typed enum variant.
NATS publish/subscribe helper functions.

**Depends on:** 1.1
**Test:** Unit tests: roundtrip serialize/deserialize every event type.
**Reference:** EVENT_CATALOG.md (all types)

---

### Task 1.3: Configuration System

**What:** Define structs for PlatformConfig and AgentConfig. Parse YAML
with serde_yaml. Validate with garde. Create sample platform.yaml and
one agent YAML. Implement `seidrum validate` subcommand.

**Depends on:** 1.1
**Test:** `seidrum validate` with valid config → success. Invalid → specific error.
**Reference:** AGENT_SPEC.md (Full Schema), TECH_STACK.md

---

### Task 1.4: Docker Compose + Infrastructure

**What:** docker-compose.yml with NATS + ArangoDB only. NATS config with
JetStream. Setup script that starts infra and waits for health.

**Depends on:** Nothing (parallel with 1.1-1.3).
**Test:** `docker-compose up -d`. NATS on 4222. ArangoDB on 8529.
**Reference:** TECH_STACK.md (Docker Compose)

---

### Task 1.5: Brain Initialization (kernel)

**What:** Implement `seidrum init`. Connect to ArangoDB. Create all
collections, edge collections, named graph, vector indexes, ArangoSearch
views, event_types collection. Seed scope_root.

**Depends on:** 1.1, 1.4
**Test:** `seidrum init`. ArangoDB web UI shows graph and indexes.
**Reference:** BRAIN_SCHEMA.md (all collections, edges, indexes)

---

### Task 1.6: Brain Service (kernel)

**What:** Implement the brain service in the kernel. Subscribe to
`brain.*.request`, `brain.content.store`, `brain.entity.upsert`,
`brain.fact.upsert`, `brain.scope.assign`, `brain.task.upsert`.
Handle each request: execute AQL, enforce scopes, publish response.
Connection pooling via reqwest to ArangoDB.

**Depends on:** 1.2, 1.5
**Test:** Publish brain.content.store via NATS CLI. Kernel stores it.
Publish brain.query.request. Kernel returns results.
**Reference:** ARCHITECTURE.md (Brain Access Pattern), BRAIN_SCHEMA.md

---

### Task 1.7: Plugin Registry (kernel)

**What:** Implement the event type registry. Subscribe to `plugin.register`.
Store plugin declarations in ArangoDB event_types collection. Validate
event schemas. Respond to registry queries.

**Depends on:** 1.2, 1.5
**Test:** Publish a plugin.register event. Query registry — plugin appears.
**Reference:** ARCHITECTURE.md (Event Type Registry), PLUGIN_SPEC.md

---

### Task 1.8: CLI Plugin

**What:** Create seidrum-cli binary. Reads stdin, publishes
`channel.cli.inbound`. Subscribes to `channel.cli.outbound`, prints to
stdout. Registers with kernel on startup.

**Depends on:** 1.2, 1.7
**Test:** Start kernel + CLI plugin. Type text. Event appears in NATS.
**Reference:** PLUGIN_SPEC.md (cli specification)

---

### Task 1.9: LLM Router Plugin (basic)

**What:** Create seidrum-llm-router binary. Consumes `agent.context.loaded`
(for now, also accept raw channel events). Calls one LLM provider (Anthropic)
via reqwest. Publishes `llm.response`. No tools, no context assembly yet —
just system prompt + user message.

**Depends on:** 1.2
**Test:** Publish an event. Get LLM response back on NATS.
**Reference:** LLM_INTEGRATION.md (Provider API, basic request/response)

---

### Task 1.10: Agent Orchestrator + End-to-End Flow

**What:** Implement the orchestrator in the kernel. Read agent YAML.
Create NATS subscription chains to wire the pipeline:
CLI inbound → llm-router → response-formatter → CLI outbound.
For now, response-formatter is a passthrough (just moves llm.response
to channel.cli.outbound).

**Depends on:** 1.3, 1.6, 1.7, 1.8, 1.9
**Test:** Start kernel + CLI + llm-router. Type "hello" in terminal.
Get LLM response. **Milestone M1: end-to-end flow.**
**Reference:** ARCHITECTURE.md (Data Flow), AGENT_SPEC.md (pipeline)

---

## Phase 2: Brain Intelligence

### Task 2.1: Content Ingester Plugin

**What:** Create seidrum-content-ingester. Consumes `channel.*.inbound`.
Publishes `brain.content.store` to kernel. Calls embedding API to generate
vectors (sent in the store request). Kernel stores content + embedding,
publishes `brain.content.stored`.

**Depends on:** 1.6
**Test:** Send message via CLI. Content appears in ArangoDB with embedding.
**Reference:** PLUGIN_SPEC.md (content-ingester), BRAIN_SCHEMA.md (content)

---

### Task 2.2: Entity Extractor Plugin

**What:** Create seidrum-entity-extractor. Consumes `brain.content.stored`.
Requests content from kernel. Calls LLM for entity extraction. Publishes
`brain.entity.upsert` for each entity. Kernel creates entities + edges.

**Depends on:** 1.6, 2.1
**Test:** Send message mentioning a person. Entity appears in ArangoDB
with mentions edge to content.
**Reference:** PLUGIN_SPEC.md (entity-extractor), BRAIN_SCHEMA.md (entities)

---

### Task 2.3: Fact Extractor Plugin

**What:** Create seidrum-fact-extractor. Consumes `brain.entity.upserted`.
Calls LLM for fact extraction. Publishes `brain.fact.upsert`. Kernel
handles contradiction detection and supersedes chains.

**Depends on:** 1.6, 2.2
**Test:** Send messages establishing then contradicting a fact. Fact chain
with supersedes edge in ArangoDB.
**Reference:** PLUGIN_SPEC.md (fact-extractor), BRAIN_SCHEMA.md (facts)

---

### Task 2.4: Graph Context Loader Plugin

**What:** Create seidrum-graph-context-loader. Consumes channel inbound
events. Uses brain.query.request for: vector search, graph traversal,
fact retrieval, task retrieval. Assembles context. Publishes
`agent.context.loaded`.

**Depends on:** 1.6, 2.1
**Test:** After ingesting several messages, send a new message referencing
a past topic. Context loader returns relevant entities and facts.
**Reference:** PLUGIN_SPEC.md (graph-context-loader), BRAIN_SCHEMA.md (queries)

---

### Task 2.5: Scope Enforcement in Kernel

**What:** Implement scope resolution in the kernel brain service. Every
brain.query.request gets scope filters injected based on the requesting
agent's scope configuration. Scope hierarchy traversal (parent scopes).

**Depends on:** 1.6, 2.4
**Test:** Create two scopes with separate entities. Query from scope A —
only scope A entities returned.
**Reference:** BRAIN_SCHEMA.md (Scope Enforcement), AGENT_SPEC.md (scope)

---

### Task 2.6: Context Assembly in LLM Router

**What:** Update llm-router to properly consume `agent.context.loaded`.
Render Tera prompt template with injected variables. Token counting and
budget allocation. Truncation per section.

**Depends on:** 1.9, 2.4
**Test:** Full pipeline with brain context. Agent recalls previously
discussed topics. **Milestone M2: persistent memory.**
**Reference:** LLM_INTEGRATION.md (Context Window Assembly)

---

## Phase 3: Channels + Tools

### Task 3.1: Telegram Plugin

**What:** Create seidrum-telegram. Uses teloxide. Bot token + user
whitelist from env. Bidirectional: channel.telegram.inbound and
channel.telegram.outbound.

**Depends on:** 1.2, 1.7
**Test:** Send message to bot on Telegram. Get response.
**Milestone M3: usable via Telegram.**
**Reference:** PLUGIN_SPEC.md (telegram)

---

### Task 3.2: Response Formatter Plugin

**What:** Create seidrum-response-formatter. Consumes `llm.response`.
Determines source channel from correlation. Formats for platform
(Telegram markdown, CLI plain text). Publishes channel.*.outbound.

**Depends on:** 1.2
**Test:** LLM response with markdown arrives formatted on Telegram.
**Reference:** PLUGIN_SPEC.md (response-formatter)

---

### Task 3.3: Tool Registry + Tool Calls

**What:** Create tools collection in ArangoDB (via kernel init). Register
built-in tools (brain-query). Update llm-router to: search tool registry,
inject tool schemas, handle tool call loop.

**Depends on:** 1.5, 2.6
**Test:** Agent uses brain-query tool to answer a question about stored data.
**Milestone M4: tools working.**
**Reference:** LLM_INTEGRATION.md (Tool Handling)

---

### Task 3.4: Task Lifecycle

**What:** Create seidrum-task-detector plugin. Kernel handles
brain.task.upsert → stores task → publishes task.created.
task.completed.{id} events wake subscribed agents.

**Depends on:** 1.6, 2.6
**Test:** Tell agent "remind me to check deployment." Task appears.
Completion event fires and agent mentions it.
**Reference:** PLUGIN_SPEC.md (task-detector), EVENT_CATALOG.md (task events)

---

## Phase 4: Polish

### Task 4.1: Scope Classifier Plugin

**What:** Create seidrum-scope-classifier. Auto-assigns content to scopes
via LLM classification.

**Depends on:** 2.5
**Reference:** PLUGIN_SPEC.md (scope-classifier)

---

### Task 4.2: Event Emitter Plugin

**What:** Create seidrum-event-emitter. Parses structured actions from
LLM responses. Emits task.created, brain.fact.upsert, agent.*.wake.

**Depends on:** 2.6
**Reference:** PLUGIN_SPEC.md (event-emitter)

---

### Task 4.3: Confidence Decay Scheduler

**What:** Implement cron scheduler in kernel using tokio-cron-scheduler.
Daily fact decay job. Publish system.maintenance.decay on completion.

**Depends on:** 1.6
**Reference:** BRAIN_SCHEMA.md (Confidence Decay)

---

### Task 4.4: Health Monitoring

**What:** Kernel periodically pings plugins on their health subjects.
Publishes system.health. Logs warnings for unresponsive plugins.

**Depends on:** 1.7
**Reference:** ARCHITECTURE.md (Plugin Lifecycle), EVENT_CATALOG.md (health)

---

### Task 4.5: Second Agent

**What:** Create a research-agent YAML with different scope, different
plugins, different LLM config. Demonstrates multi-agent architecture.

**Depends on:** Phase 3 complete
**Reference:** AGENT_SPEC.md

---

### Task 4.6: Validate Command (comprehensive)

**What:** Enhance `seidrum validate`: check agent YAML chains, prompt
files exist, scopes exist in brain, plugins registered, pipeline coherent.

**Depends on:** 1.3, 1.7, 2.5
**Reference:** ARCHITECTURE.md (orchestrator validation)

---

### Task 4.7: Dockerfiles + Release

**What:** Multi-stage Dockerfile for kernel and each plugin. Docker Compose
with all services. Setup script for first-time deployment. README.

**Depends on:** All prior tasks.
**Test:** `docker-compose up` brings up entire Seidrum stack.
**Milestone M5: production-ready deployment.**
**Reference:** TECH_STACK.md (Dockerfile Pattern)

---

## Milestones

| Milestone | After Task | Description                                       |
|-----------|------------|---------------------------------------------------|
| M1        | 1.10       | End-to-end: CLI → NATS → LLM → CLI               |
| M2        | 2.6        | Agent has persistent memory from knowledge graph   |
| M3        | 3.1        | Usable via Telegram with full brain pipeline       |
| M4        | 3.3        | Tools and tool registry working                    |
| M5        | 4.7        | Full Docker deployment with all plugins            |
