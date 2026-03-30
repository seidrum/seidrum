# Architectural Decision Records: Phase 1-3 Evolution

**Date:** March 30, 2026
**Scope:** Seidrum event-driven personal AI agent platform (Phase 1-3 development)

This document records the key architectural decisions made during the Phase 1-3 evolution of Seidrum, focusing on semantic search capabilities, feedback loops, multi-provider LLM support, preference injection, and plugin structure.

---

## ADR-001: Embedding Pipeline Architecture

**Status:** Accepted

### Context

Seidrum needed semantic search capabilities to enable the agent to find relevant facts and content based on similarity, not just exact keyword matches. The brain service stores all content and entities in ArangoDB, but the initial design had no vector search layer.

### Decision

Wire the existing `EmbeddingService` (OpenAI text-embedding-3-small, 1536 dimensions) directly into the brain service's content ingestion and entity upsert handlers. Use ArangoDB's native vector indexes (MDI-prefixed, cosine metric) rather than a separate vector database.

### Rationale

- **Single database:** ArangoDB 3.12+ supports native vector indexes, avoiding the operational complexity of a separate vector store (Pinecone, Weaviate, etc.). All data and vectors live in one system.
- **Inline generation:** Embeddings are generated non-blocking during content storage. If embedding fails (API timeout, quota exceeded), the document is still stored; the vector field is empty and retried asynchronously.
- **Hybrid search:** The system can combine vector similarity queries with graph traversal in the same database, leveraging ArangoDB's unique strength as a multi-model database.
- **Scope-aware:** Embeddings participate in existing scope filtering—queries respect the agent's data scope automatically.

### Tradeoffs

- **Operational maturity:** ArangoDB's vector indexing is less mature than dedicated vector databases. Edge cases in scaling may emerge.
- **OpenAI dependency:** Embedding generation is tied to OpenAI's text-embedding-3-small. Future support for local embeddings (Ollama) requires additional abstraction.
- **Fixed dimensionality:** 1536 dimensions is locked to the chosen model. Switching models requires re-embedding all existing content.
- **Storage cost:** Storing embeddings inline increases ArangoDB disk usage by ~6KB per document (1536 floats × 4 bytes).

---

## ADR-002: Feedback Loop as Event-Driven Pipeline

**Status:** Accepted

### Context

Agents needed to learn from user corrections, preferences, and implicit feedback across sessions. The system required a mechanism to capture this feedback and apply it to future interactions without manual intervention.

### Decision

Implement feedback as a standard Seidrum plugin (`seidrum-feedback-extractor`) that emits `agent.feedback` events, consumed by the consciousness service and stored as brain facts. Use heuristic pattern matching (not LLM-based detection) as the first implementation to minimize inference costs.

### Rationale

- **Architectural consistency:** Feedback follows the existing plugin pattern—an independent process that subscribes to events and publishes new events. No special-case handling in the kernel.
- **Low cost:** Heuristic detection (regex, keyword matching, sentiment signals) avoids LLM calls for every user message, keeping feedback extraction cheap.
- **Knowledge graph integration:** Feedback stored as brain facts means it participates in confidence decay, time-to-live management, and scope filtering. Preferences are queryable like any other fact.
- **Dynamic injection:** Preferences are injected into the system prompt per request, not hardcoded. Agents see updated preferences without restart.

### Tradeoffs

- **Detection accuracy:** Heuristics will miss subtle feedback (sarcasm, implicit preferences, multi-turn corrections). Sentence like "hmm, interesting" could be positive or dismissive.
- **No explicit UI:** The system relies on natural language feedback in the conversation. No thumbs-up/thumbs-down buttons or dedicated feedback endpoint.
- **Conflict resolution:** No weighting or priority system yet for conflicting preferences (e.g., "be verbose" followed by "be concise"). Last-updated-wins semantics.
- **False positives:** Feedback extraction may trigger on unintended phrases, storing incorrect preferences.

---

## ADR-003: Multi-Provider LLM with Intelligent Routing

**Status:** Accepted

### Context

The system initially supported only Google Gemini. Expanding to multiple LLM providers enables:
- Cost optimization (use cheaper models for simple tasks)
- Resilience (fallback if primary provider fails)
- Privacy/compliance (use local models or specific providers per scope)
- Feature access (some models have tool calling, long context, etc.)

### Decision

Create separate plugin crates for each LLM provider (seidrum-llm-openai, seidrum-llm-anthropic, seidrum-llm-ollama) and upgrade the LLM router with a multi-strategy routing system. Routing strategies include: fixed (always use provider X), fallback (try primary, then secondary on failure), and intelligent (select provider based on request metadata: model preference, privacy scope, complexity, tool density).

### Rationale

- **Plugin architecture:** Each provider is a separate Rust binary, matching Seidrum's design principle that everything is a plugin. Independent deployment, independent scaling.
- **Encapsulation:** Provider plugins handle their own tool call loop and retry logic internally. The consciousness service is agnostic to provider details.
- **Resilience:** Fallback chains allow graceful degradation. If OpenAI API is rate-limited, the system automatically tries Anthropic or local Ollama.
- **Privacy:** Agents can specify scope-based provider preferences—e.g., "use only local Ollama for employee data."
- **Future-proof:** New providers can be added as new crates without touching kernel code.

### Tradeoffs

- **Code duplication:** Each provider plugin (~750 LOC) has similar structure but different API translation layers. No shared provider abstraction because the APIs differ significantly (tool calling models, token counting, function schemas).
- **Consistency risk:** Different providers have different semantics (token limits, tool availability, behavior differences). Tests must cover multi-provider scenarios.
- **Operational complexity:** Managing 4-5 provider plugins (each with its own config, credentials, health checks) increases deployment surface.
- **Ollama assumption:** The Ollama provider assumes local deployment without authentication. Using a remote Ollama instance requires extending the plugin.

---

## ADR-004: Consciousness Preference Injection

**Status:** Accepted

### Context

Learned preferences (from feedback) needed to influence the agent's behavior on every request. The consciousness service orchestrates all agent activity and calls LLMs, making it the natural place to apply preferences.

### Decision

Before each LLM call, the consciousness service queries for preferences via NATS request/reply (with timeout). Retrieved preferences are formatted as a structured section in the system prompt and injected before sending the request to the LLM provider. The consciousness service also subscribes to `agent.feedback` events and stores new preferences as brain facts.

### Rationale

- **Simplicity:** System prompt injection works with any LLM provider (OpenAI, Anthropic, local Ollama). No provider-specific preference handling required.
- **Non-blocking:** NATS request/reply with a 5-second timeout ensures preferences don't delay conversation flow if the brain service is unavailable.
- **Persistence:** Preferences stored as brain facts participate in confidence decay (stale preferences fade) and scope filtering (preferences are scoped to agent + user).
- **Queryability:** Preferences are queryable via the brain API, enabling UI display ("Your preferences: be concise, explain technical jargon").

### Tradeoffs

- **Token cost:** System prompt injection adds tokens to every request, even when preferences are stable. Could increase API costs 5-10% depending on preference size.
- **No priority system:** Conflicting preferences (e.g., "be verbose" and "be concise") have no explicit weighting. First-stored or last-updated semantics apply.
- **Timeout tuning:** 5-second preference query timeout may be too generous for real-time conversations or too strict for high-latency networks.
- **Prompt injection risk:** Preferences injected into system prompt could theoretically be abused if a user crafts preferences that contain prompt-like instructions.

---

## ADR-005: Plugin Crate Structure (Full Standalone)

**Status:** Accepted

### Context

New functionality required by Phase 1-3 (feedback extraction, LLM provider support) could either be added to existing crates or organized as new standalone crates. This decision affects workspace structure, deployment, and team organization.

### Decision

Each new component is a full standalone crate with its own `Cargo.toml`, binary target, and `Dockerfile`. This includes the feedback extractor and each LLM provider plugin (OpenAI, Anthropic, Ollama).

### Rationale

- **Consistency:** Matches the existing Seidrum convention where each plugin is a separate crate (kernel, telegram, cli, api-gateway, discord, etc.). 19 plugins established a precedent.
- **Independent deployment:** Each plugin can be started, stopped, redeployed, or scaled independently without touching other components. Critical for production resilience.
- **Clear ownership:** Each crate has a single, well-defined responsibility (execute a role in the event pipeline). Reduces cross-cutting concerns.
- **Docker composability:** Each crate gets a `Dockerfile` and service entry in `docker-compose.yml`. Enables easy local development and Kubernetes deployment.
- **Isolation:** Dependency conflicts between plugins are impossible (each has its own dependency tree). Simplifies version management.

### Tradeoffs

- **Workspace growth:** Phase 1-3 grows the workspace from 19 crates to 27 crates (feedback extractor, 4 LLM providers, and related). More to manage and test.
- **Boilerplate:** Every new plugin includes repetitive code (NATS connect, plugin registration, event loop, error handling, tracing). No shared plugin template yet to reduce duplication.
- **Compilation time:** Workspace size increases build times. `cargo build --workspace` now takes longer; developers must use `cargo build -p <crate>` for faster iteration.
- **Dependency duplication:** Common dependencies (tokio, serde, tracing, anyhow) appear in each crate's `Cargo.toml`. Workspace `[dependencies]` helps but doesn't eliminate redundancy.

---

## Summary

These five decisions establish the foundation for Seidrum's Phase 1-3 feature set:

1. **Semantic search** via native ArangoDB vector indexes
2. **Feedback learning** as an event-driven plugin pipeline
3. **Multi-provider resilience** with intelligent LLM routing
4. **Dynamic preference injection** into agent prompts
5. **Plugin-based organization** as the standard architecture

All decisions accepted on **March 30, 2026** and implemented as of Phase 1-3.
