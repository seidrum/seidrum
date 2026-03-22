# PROJECT.md — Seidrum

## What This Is

Seidrum is an event-driven, intelligence-agnostic, channel-agnostic personal
AI agent platform. It connects LLMs to the user's digital life (emails,
messages, files, calendar) through a persistent knowledge graph that serves
as the agent's brain.

At its core is NATS JetStream as the nervous system and ArangoDB as the brain.
There is no distinction between channels, LLM providers, or processing logic —
they are all plugins: independent processes that declare what event types they
consume and produce. Telegram is a plugin. The LLM is a plugin. Entity
extraction is a plugin.

The kernel is minimal. It owns the brain (ArangoDB connection + scope
enforcement), the event type registry (schema validation), and the agent
orchestrator (reads YAML, wires plugin pipelines). Everything else lives
outside the kernel as independent plugin processes communicating through NATS.

The platform runs self-hosted on a single machine. The kernel is a single
Rust binary. Plugins can be written in any language that speaks NATS.

## Problems It Solves

1. **Memory is unscoped and degrades over time.** Existing assistants have
   flat, append-only memory. Context from different life areas bleeds together.
   The more you use it, the noisier it gets. Seidrum uses a scoped, temporal
   knowledge graph where facts have confidence, provenance, and expiration.

2. **Long-running tasks are forgotten.** Agents say "I'll do this and let you
   know" but have no mechanism to actually come back. Seidrum uses persistent
   task objects and NATS events so that task completion wakes the agent and
   routes the result back to the user on the right channel.

3. **LLM lock-in.** Users should bring their own API keys and route between
   providers based on cost, speed, or quality. LLM providers are just plugins
   that consume `llm.request.*` events and produce `llm.response` events.

4. **No separation between tools and middleware.** Seidrum distinguishes
   tools (LLM-invoked, described, optional) from plugins (event processors,
   always-on). Tools live in the LLM's decision space. Plugins live in the
   event pipeline.

5. **Monolithic architectures resist extension.** In Seidrum, adding a new
   channel, a new LLM provider, or a new processing step means writing a
   small process that speaks NATS. No kernel changes. No recompilation.

## Design Principles

- **Events are the universal primitive.** Every interaction is a NATS event.
  User message, LLM response, task completion, plugin output, agent-to-agent
  message — all events. The system routes events, not function calls.

- **Everything is a plugin.** There is no architectural distinction between a
  Telegram adapter, an LLM provider, an entity extractor, or a response
  formatter. They are all processes that consume and produce NATS events.

- **The brain is a graph, not a log.** Relationships between entities matter
  more than chronological message history. The graph enables traversal:
  "Alice mentioned a tool in a Telegram message to Bob, referencing a
  GitHub repo" — that's a graph query.

- **Scopes are boundaries.** An agent operating in the "job search" scope
  cannot see "personal finance" data unless explicitly granted cross-scope
  access. Scopes are enforced by the kernel on every brain query.

- **Facts are temporal.** The system tracks not just what is true, but when
  it became true, how confident we are, and what content it was derived from.
  Old facts decay. New content reinforces or supersedes existing facts.

- **Plugins never touch the brain directly.** All brain access goes through
  the kernel via NATS request/reply. The kernel enforces scopes on every
  query. No plugin can bypass scoping.

- **Minimize the kernel.** The kernel owns brain, registry, and orchestration.
  Everything else is a plugin. If it can be a plugin, it must be a plugin.

## Glossary

| Term         | Definition                                                     |
|--------------|----------------------------------------------------------------|
| Agent        | A declared pipeline of plugins wired together via event types. Defined in YAML. |
| Brain        | The ArangoDB knowledge graph. Contains entities, content, facts, scopes, and tasks. |
| Channel      | A messaging platform (Telegram, CLI, Web). Implemented as a bidirectional plugin. |
| Content      | Immutable raw data ingested into the brain: messages, emails, docs, files. |
| Entity       | A persistent node in the brain graph: person, organization, project, tool, etc. |
| Event        | A typed NATS message. The universal communication primitive. |
| Fact         | A temporal knowledge claim extracted from content. Has confidence, validity window, and provenance. |
| Kernel       | The core Rust binary. Owns brain access, event type registry, and agent orchestration. |
| Plugin       | Any independent process that consumes and produces NATS events. Everything outside the kernel is a plugin. |
| Scope        | A context boundary in the brain. Restricts what an agent can see and traverse. |
| Task         | A persistent actionable item with lifecycle. Completion emits a NATS event. |
| Tool         | A capability described to the LLM. Can be MCP server, CLI, or built-in. LLM decides when to invoke. |
| Tool Registry| ArangoDB collection with vector-indexed tool descriptions. Agents search it via a meta-tool. |

## Name

**Seidrum** — from Old Norse *seidr* (the practice of seeing hidden connections
and perceiving the web of fate) + *rum* (space, room). "The space where hidden
connections are seen." Pronounced SAY-drum.
