# EVENT_CATALOG.md — Seidrum Event Types and Payloads

## Overview

Every interaction in Seidrum is a NATS event. Events are JSON payloads
published to NATS subjects following: `{domain}.{resource}.{action}`.

All events share a common envelope:

```rust
#[derive(Serialize, Deserialize)]
struct EventEnvelope {
    id: String,                      // ULID
    event_type: String,              // matches NATS subject
    timestamp: DateTime<Utc>,
    source: String,                  // plugin that emitted this
    correlation_id: Option<String>,  // links related events
    scope: Option<String>,           // scope context
    payload: serde_json::Value,      // event-specific data
}
```

---

## Channel Events

### `channel.{platform}.inbound`

Incoming user message from any channel plugin.

```rust
struct ChannelInbound {
    platform: String,        // "telegram" | "cli" | "web"
    user_id: String,
    chat_id: String,
    text: String,
    reply_to: Option<String>,
    attachments: Vec<Attachment>,
    metadata: HashMap<String, String>,
}

struct Attachment {
    file_type: String,       // "image" | "document" | "voice" | "video"
    url: Option<String>,
    file_id: Option<String>,
    mime_type: String,
    size_bytes: u64,
}
```

### `channel.{platform}.outbound`

Response from agent back to a channel plugin.

```rust
struct ChannelOutbound {
    platform: String,
    chat_id: String,
    text: String,
    format: String,          // "plain" | "markdown" | "html"
    reply_to: Option<String>,
    actions: Vec<ChannelAction>,
}

struct ChannelAction {
    label: String,
    action_type: String,     // "button" | "link" | "callback"
    value: String,
}
```

---

## Brain Events (Kernel ↔ Plugin communication)

### `brain.content.store` (plugin → kernel)

Request to store raw content.

```rust
struct ContentStoreRequest {
    content_type: String,    // "message" | "email" | "document" etc.
    channel: String,
    channel_id: String,
    raw_text: String,
    timestamp: DateTime<Utc>,
    metadata: HashMap<String, String>,
    generate_embedding: bool,
}
```

### `brain.content.stored` (kernel → plugins)

Content successfully stored.

```rust
struct ContentStored {
    content_key: String,
    content_type: String,
    channel: String,
    embedding_generated: bool,
    timestamp: DateTime<Utc>,
}
```

### `brain.entity.upsert` (plugin → kernel)

Create or update an entity.

```rust
struct EntityUpsertRequest {
    entity_key: Option<String>,  // None = create new, Some = update
    entity_type: String,
    name: String,
    aliases: Vec<String>,
    properties: HashMap<String, String>,
    source_content: Option<String>,
    mentions_content: Option<String>,  // content key to create mentions edge
    mention_type: Option<String>,
}
```

### `brain.entity.upserted` (kernel → plugins)

Entity created or updated.

```rust
struct EntityUpserted {
    entity_key: String,
    entity_type: String,
    name: String,
    is_new: bool,
    source_content: Option<String>,
}
```

### `brain.fact.upsert` (plugin → kernel)

Create or update a fact.

```rust
struct FactUpsertRequest {
    subject: String,         // entity _id
    predicate: String,
    object: Option<String>,  // entity _id
    value: Option<String>,   // literal value
    confidence: f64,
    source_content: String,
    valid_from: Option<DateTime<Utc>>,
}
```

### `brain.fact.upserted` (kernel → plugins)

Fact stored. Kernel handles contradiction detection and supersedes chains.

```rust
struct FactUpserted {
    fact_key: String,
    subject: String,
    predicate: String,
    is_new: bool,
    superseded_fact: Option<String>,  // key of fact that was superseded
}
```

### `brain.scope.assign` (plugin → kernel)

Assign content/entity to a scope.

```rust
struct ScopeAssignRequest {
    target_key: String,      // entity or content _id
    scope_key: String,
    relevance: f64,
}
```

### `brain.scope.assigned` (kernel → plugins)

```rust
struct ScopeAssigned {
    target_key: String,
    scope_key: String,
}
```

### `brain.query.request` (plugin → kernel, request/reply)

Read query against the brain. Uses NATS request/reply pattern.

```rust
struct BrainQueryRequest {
    query_type: String,      // "aql" | "vector_search" | "graph_traverse"
                             // | "get_facts" | "get_context"
    // For AQL:
    aql: Option<String>,
    bind_vars: Option<HashMap<String, serde_json::Value>>,
    // For vector search:
    embedding: Option<Vec<f64>>,
    collection: Option<String>,
    limit: Option<u32>,
    // For graph traversal:
    start_vertex: Option<String>,
    direction: Option<String>,  // "outbound" | "inbound" | "any"
    depth: Option<u32>,
    // For get_context (high-level convenience):
    query_text: Option<String>,
    max_facts: Option<u32>,
    graph_depth: Option<u32>,
    min_confidence: Option<f64>,
    // Scope is injected by kernel from the requesting agent's config
}
```

### `brain.query.response` (kernel → plugin, reply)

```rust
struct BrainQueryResponse {
    results: serde_json::Value,  // query-specific result shape
    count: u32,
    scopes_applied: Vec<String>,
    duration_ms: u64,
}
```

---

## Agent Events

### `agent.context.loaded`

Graph context loader has assembled context for a message.

```rust
struct AgentContextLoaded {
    original_event: EventEnvelope,   // the triggering event
    entities: Vec<serde_json::Value>,
    facts: Vec<serde_json::Value>,
    similar_content: Vec<serde_json::Value>,
    active_tasks: Vec<serde_json::Value>,
    conversation_history: Vec<serde_json::Value>,
}
```

### `agent.{agent_id}.wake`

Explicitly wake an agent.

```rust
struct AgentWake {
    agent_id: String,
    reason: String,
    context: HashMap<String, String>,
}
```

---

## LLM Events

### `llm.request.{provider}` or `llm.request.auto`

Request for LLM completion.

```rust
struct LlmRequest {
    agent_id: String,
    messages: Vec<LlmMessage>,
    model: Option<String>,        // specific model or None for auto
    temperature: f64,
    max_tokens: u32,
    tools: Option<Vec<ToolSchema>>,
    tool_choice: Option<String>,
    routing_strategy: String,     // "best-first" | "cheap-first" etc.
    model_preferences: Vec<String>,
}

struct LlmMessage {
    role: String,
    content: String,
    name: Option<String>,
    tool_call_id: Option<String>,
}

struct ToolSchema {
    name: String,
    description: String,
    parameters: serde_json::Value,
}
```

### `llm.response`

LLM completion result.

```rust
struct LlmResponse {
    agent_id: String,
    content: Option<String>,
    tool_calls: Option<Vec<ToolCall>>,
    model_used: String,
    provider: String,
    tokens: TokenUsage,
    duration_ms: u64,
    finish_reason: String,
}

struct ToolCall {
    id: String,
    function_name: String,
    arguments: String,  // JSON string
}

struct TokenUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
    estimated_cost_usd: f64,
}
```

---

## Task Events

### `task.created`

```rust
struct TaskCreated {
    task_key: String,
    title: String,
    description: Option<String>,
    priority: String,
    assigned_agent: Option<String>,
    due_date: Option<DateTime<Utc>>,
    callback_channel: Option<String>,
    scope: String,
}
```

### `task.updated`

```rust
struct TaskUpdated {
    task_key: String,
    old_status: String,
    new_status: String,
    update_reason: Option<String>,
}
```

### `task.completed.{task_id}`

```rust
struct TaskCompleted {
    task_key: String,
    result: Option<String>,
    duration_ms: u64,
    callback_channel: Option<String>,
}
```

---

## Plugin Events

### `plugin.register`

Plugin announces itself to the kernel.

```rust
struct PluginRegister {
    id: String,
    name: String,
    version: String,
    description: String,
    consumes: Vec<String>,   // NATS subjects
    produces: Vec<String>,   // NATS subjects
    health_subject: String,  // e.g., "plugin.telegram.health"
}
```

### `plugin.{id}.health` (request/reply)

```rust
struct PluginHealthRequest {}

struct PluginHealthResponse {
    plugin_id: String,
    status: String,          // "healthy" | "degraded" | "unhealthy"
    uptime_seconds: u64,
    events_processed: u64,
    last_error: Option<String>,
}
```

### `plugin.{id}.error`

```rust
struct PluginError {
    plugin_id: String,
    error_type: String,
    message: String,
    event_id: String,
    recoverable: bool,
}
```

---

## System Events

### `system.health`

Published periodically by kernel scheduler.

```rust
struct SystemHealth {
    nats_connected: bool,
    arangodb_connected: bool,
    active_plugins: Vec<String>,
    active_agents: u32,
    uptime_seconds: u64,
}
```

### `system.maintenance.decay`

```rust
struct DecayCompleted {
    facts_decayed: u32,
    facts_archived: u32,
    duration_ms: u64,
}
```
