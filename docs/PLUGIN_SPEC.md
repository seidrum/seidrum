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

### Capability Registration

Plugins can register capabilities during bootstrap by publishing a
`capability.register` event:

- **kind: "tool"** — Available to the LLM via the capability registry for
  tool-use calls.
- **kind: "command"** — Auto-discovered by the Telegram plugin as `/` commands
  for direct user invocation.
- **kind: "both"** — Registered as both a tool and a command.

Capabilities with kind `"command"` or `"both"` are automatically surfaced by
the Telegram plugin via `setMyCommands`, so users see them in the command menu.
Capabilities with kind `"tool"` or `"both"` are made available to the LLM
through the capability registry, allowing the model to invoke them during
tool-use rounds.

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

### tool-dispatcher

```
Consumes: capability.call
Produces: (forwards to owning plugin via NATS request/reply)

Behavior:
1. Subscribe to capability.call events
2. Look up the target capability from an in-memory cache of registered
   capabilities (populated from capability.register events)
3. Forward the call payload to the owning plugin's call_subject via
   NATS request/reply with a 30s timeout
4. Return the plugin's response to the original caller
5. If the target capability is unknown or the owning plugin does not
   respond within the timeout, return an error payload

Config:
  timeout: NATS request/reply timeout (default: 30s)
```

### code-executor

```
Consumes: capability.call.code-executor
Produces: capability.call.code-executor (reply)

Behavior:
1. Registers as a capability with kind "both" and
   call_subject capability.call.code-executor
2. Receives execution requests containing language and code
3. Supported languages: Python, Bash, JavaScript
4. Spawns a sandboxed subprocess for the requested language
5. Network isolation via Linux unshare (CLONE_NEWNET) to prevent
   untrusted code from making network calls
6. Captures stdout, stderr, and exit code
7. Enforces a hard timeout of 30s per execution — kills the process
   if exceeded
8. Returns structured result with stdout, stderr, exit_code, and
   timed_out flag

Config:
  max_timeout: maximum execution time per request (default: 30s)
  allowed_languages: list of enabled languages (default: [python, bash, javascript])
```

### claude-code

```
Consumes: capability.call.claude-code
Produces: capability.call.claude-code (reply)

Behavior:
1. Registers as a capability with kind "both" and
   command_alias "claude"
2. Receives coding requests with a prompt and optional working_dir
3. Spawns `claude -p` with --output-format json, passing the prompt
   via stdin
4. Streams output and collects the final JSON result
5. Supports per-request working_dir to scope the Claude Code session
   to a specific project directory
6. Enforces a configurable timeout (default: 5 minutes) — kills the
   process if exceeded
7. Returns structured result with the Claude Code JSON output,
   exit_code, and timed_out flag

Config:
  timeout: maximum execution time per request (default: 5m)
  working_dir: default working directory if not specified per-request
  claude_bin: path to the claude CLI binary (default: "claude")
```

### notification

```
Consumes: notification.send
Produces: channel.{target}.outbound

Behavior:
1. Receives notification.send events containing a message, importance
   level, and optional target channel override
2. Evaluates importance level against configured thresholds to decide
   whether to deliver or suppress the notification
3. Routes the notification to the appropriate channel by publishing a
   channel.{target}.outbound event
4. If no target is specified, uses the default channel from config
5. Supports importance levels: low, normal, high, critical
6. Critical notifications are always delivered regardless of quiet
   hours or suppression rules

Config:
  default_channel: default output channel (e.g., "telegram")
  min_importance: minimum importance level to deliver (default: normal)
```

### email (Channel Plugin)

```
Consumes: channel.email.outbound
Produces: channel.email.inbound

Behavior:
1. Connects to IMAP server and polls for new messages at a
   configurable interval
2. New emails → channel.email.inbound events with sender, subject,
   body, and attachment metadata
3. channel.email.outbound events → sends email via SMTP with
   subject, body, recipients, and optional attachments
4. Tracks seen message UIDs to avoid re-processing
5. Supports TLS for both IMAP and SMTP connections
6. Bidirectional channel plugin: both receives and sends

Config:
  imap_host: IMAP server hostname
  imap_port: IMAP server port (default: 993)
  smtp_host: SMTP server hostname
  smtp_port: SMTP server port (default: 587)
  poll_interval: how often to check for new mail (default: 60s)

Env:
  - EMAIL_ADDRESS
  - EMAIL_PASSWORD
```

### calendar

```
Consumes: capability.call.search-calendar
Produces: calendar.event.upcoming, capability.call.search-calendar (reply)

Behavior:
1. Connects to Google Calendar API via OAuth2 service account
2. Polls for upcoming events at a configurable interval and publishes
   calendar.event.upcoming events for events starting within the
   lookahead window
3. Registers a search-calendar capability with kind "both" so the LLM
   and users can query the calendar
4. Search requests accept a date range and optional query string,
   returning matching calendar events
5. Caches calendar data locally to reduce API calls

Config:
  poll_interval: how often to check for upcoming events (default: 5m)
  lookahead: time window for upcoming event notifications (default: 1h)
  calendars: list of calendar IDs to monitor

Env:
  - GOOGLE_CALENDAR_CREDENTIALS (service account JSON)
```

### Plugin Storage (Kernel Service)

> **Note:** Plugin storage is not a plugin — it is a kernel-provided service.
> It offers a persistent key-value store that any plugin can use via NATS
> request/reply. This avoids plugins needing their own database connections.
>
> **Subjects:**
> - `storage.get` — Retrieve a value by plugin ID and key
> - `storage.set` — Store or update a value by plugin ID and key
> - `storage.delete` — Remove a value by plugin ID and key
> - `storage.list` — List all keys (with optional prefix filter) for a plugin
>
> **Backing store:** ArangoDB `plugin_storage` collection, keyed by
> `(plugin_id, key)`. Values are stored as arbitrary JSON. The kernel handles
> serialization, TTL expiry (if set), and collection management.
>
> Plugins use this for persisting state across restarts — e.g., conversation
> context, user preferences, polling cursors, or cached data.

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
