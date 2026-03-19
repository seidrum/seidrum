# PLUGIN_SPEC.md — Seidrum Plugin System

## Core Principle

Everything outside the kernel is a plugin. There is one plugin interface.
A plugin is a process that:

1. Connects to NATS
2. Registers itself (declares what it consumes and produces)
3. Subscribes to its declared event types
4. Processes events and publishes results
5. Responds to health checks

That's it. Telegram, LLM routing, entity extraction, response formatting —
all the same interface.

## Plugin Declaration

Every plugin declares itself via a YAML file and registers with the kernel
at startup:

```yaml
# Plugin declaration (also sent as plugin.register event)
plugin:
  id: telegram
  name: Telegram Channel
  version: 0.1.0
  description: Bridges Telegram Bot API to NATS events

  consumes:
    - channel.telegram.outbound

  produces:
    - channel.telegram.inbound

  runtime:
    type: docker
    image: seidrum/telegram:latest

  env:
    - TELEGRAM_TOKEN
    - TELEGRAM_ALLOWED_USERS

  health:
    interval: 30s
    subject: plugin.telegram.health
```

## Plugin Bootstrap (Rust)

All Seidrum plugins share a common startup pattern via `seidrum-common`:

```rust
use seidrum_common::{PluginConfig, NatsClient, EventEnvelope};

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Load config
    let config = PluginConfig::from_env()?;

    // 2. Connect to NATS
    let nats = NatsClient::connect(&config.nats_url).await?;

    // 3. Register with kernel
    nats.publish("plugin.register", PluginRegister {
        id: "telegram".into(),
        name: "Telegram Channel".into(),
        version: "0.1.0".into(),
        consumes: vec!["channel.telegram.outbound".into()],
        produces: vec!["channel.telegram.inbound".into()],
        health_subject: "plugin.telegram.health".into(),
    }).await?;

    // 4. Subscribe to consumed events
    let mut sub = nats.subscribe("channel.telegram.outbound").await?;

    // 5. Process events
    while let Some(msg) = sub.next().await {
        let event: EventEnvelope = serde_json::from_slice(&msg.payload)?;
        // ... process and publish results ...
    }

    Ok(())
}
```

## Brain Access from Plugins

Plugins never connect to ArangoDB. They use NATS request/reply to the kernel:

```rust
// Read: query the brain
let response = nats.request("brain.query.request", BrainQueryRequest {
    query_type: "get_context".into(),
    query_text: Some("Infrahub interview status".into()),
    max_facts: Some(20),
    graph_depth: Some(2),
    min_confidence: Some(0.5),
    ..Default::default()
}).await?;

let brain_result: BrainQueryResponse = serde_json::from_slice(&response.payload)?;

// Write: store content
nats.publish("brain.content.store", ContentStoreRequest {
    content_type: "message".into(),
    channel: "telegram".into(),
    raw_text: msg.text.clone(),
    generate_embedding: true,
    ..Default::default()
}).await?;
// Kernel will publish brain.content.stored when done
```

## Built-in Plugin Specifications

### telegram (Channel Plugin)

```
Consumes: channel.telegram.outbound
Produces: channel.telegram.inbound

Behavior:
- Connects to Telegram Bot API via teloxide
- Incoming messages → channel.telegram.inbound events
- channel.telegram.outbound events → Telegram Bot API
- Handles markdown formatting, inline keyboards, media
- User ID whitelist for access control
```

### cli (Channel Plugin)

```
Consumes: channel.cli.outbound
Produces: channel.cli.inbound

Behavior:
- Reads stdin line by line
- Lines → channel.cli.inbound events
- channel.cli.outbound events → stdout with ANSI formatting
- Development/testing only
```

### content-ingester

```
Consumes: channel.*.inbound (any channel inbound event)
Produces: brain.content.store → (kernel handles) → brain.content.stored

Behavior:
1. Extract text from channel event
2. Publish brain.content.store request to kernel
3. Kernel stores content, generates embedding, publishes brain.content.stored
4. Pass original event through (does not block pipeline)
```

### graph-context-loader

```
Consumes: channel.*.inbound (any channel inbound event)
Produces: agent.context.loaded

Behavior:
1. Extract text from event
2. Request brain: vector search for similar content
3. Request brain: graph traversal to expand entity neighborhood
4. Request brain: current facts for discovered entities
5. Request brain: active tasks for current agent
6. Request brain: recent conversation history
7. Assemble all context into agent.context.loaded event

Config:
  graph_depth: how many hops to traverse (default: 3)
  max_facts: maximum facts to include (default: 50)
  min_confidence: fact confidence threshold (default: 0.5)
  conversation_history_length: recent messages (default: 20)
```

### llm-router

```
Consumes: agent.context.loaded
Produces: llm.response

Behavior:
1. Receive context-loaded event
2. Render prompt template with Tera (inject facts, history, tasks, tools)
3. Count tokens, apply budget allocation, truncate sections
4. Select model based on routing strategy:
   - best-first: use first model in preference list
   - cheap-first: use cheapest model, fall back to expensive
   - fast-first: track latency, route to fastest
   - fallback: try models in order on failure
5. Call LLM provider API directly (reqwest):
   - Anthropic: POST https://api.anthropic.com/v1/messages
   - OpenAI: POST https://api.openai.com/v1/chat/completions
   - Ollama: POST http://{host}:11434/api/chat
6. Handle tool calls: execute → feed result → call again (max 10 rounds)
7. Publish llm.response event

Config:
  strategy: routing strategy name
  models: ordered model preference list
  max_tokens: response token limit
  temperature: sampling temperature
  max_context_tokens: total context budget
  prompt: path to Tera template
  tools: tool registry and pinned tools config
```

### entity-extractor

```
Consumes: brain.content.stored
Produces: brain.entity.upsert

Behavior:
1. Request brain: get content text by key
2. Call LLM with entity extraction prompt
3. For each extracted entity:
   a. Request brain: fuzzy search for existing entity
   b. Publish brain.entity.upsert (kernel handles create/update + edges)
```

### fact-extractor

```
Consumes: brain.entity.upserted
Produces: brain.fact.upsert

Behavior:
1. Request brain: get content and entities for this extraction
2. Call LLM with fact extraction prompt
3. For each extracted fact:
   a. Publish brain.fact.upsert
   b. Kernel handles contradiction detection and supersedes chains
```

### scope-classifier

```
Consumes: brain.content.stored
Produces: brain.scope.assign

Behavior:
1. Request brain: get content text
2. Request brain: list active scopes with descriptions
3. Call LLM: classify content into scopes with relevance scores
4. Publish brain.scope.assign for each scope match
```

### response-formatter

```
Consumes: llm.response
Produces: channel.{platform}.outbound

Behavior:
1. Determine target channel from correlation_id / original event
2. Format LLM response for target platform:
   - Telegram: convert markdown, add buttons
   - CLI: plain text with ANSI
3. Publish channel.{platform}.outbound
```

### event-emitter

```
Consumes: llm.response
Produces: task.created, brain.fact.upsert, agent.{id}.wake

Behavior:
1. Parse LLM response for structured markers/JSON blocks
2. Emit corresponding events for detected actions
3. Pass original response through
```

### task-detector

```
Consumes: llm.response
Produces: task.created

Behavior:
1. Call LLM with task detection prompt
2. For each detected task: publish brain.task.upsert to kernel
3. Kernel stores task and publishes task.created
```

## Writing a Custom Plugin

A custom plugin in any language follows this pattern:

1. Connect to NATS at the configured URL
2. Publish a `plugin.register` event with your declaration
3. Subscribe to your declared `consumes` subjects
4. For each received event, process and publish to your `produces` subjects
5. Respond to health pings on your declared health subject

Example in Python:

```python
import nats
import json

async def main():
    nc = await nats.connect("nats://nats:4222")

    # Register
    await nc.publish("plugin.register", json.dumps({
        "id": "my-custom-plugin",
        "name": "My Plugin",
        "version": "0.1.0",
        "consumes": ["brain.content.stored"],
        "produces": ["custom.analysis.complete"],
        "health_subject": "plugin.my-custom-plugin.health"
    }).encode())

    # Subscribe and process
    sub = await nc.subscribe("brain.content.stored")
    async for msg in sub.messages:
        event = json.loads(msg.data)
        result = my_processing_logic(event)
        await nc.publish("custom.analysis.complete",
                         json.dumps(result).encode())
```

This works because the only contract is: connect to NATS, consume events,
produce events. Language doesn't matter. Runtime doesn't matter.
