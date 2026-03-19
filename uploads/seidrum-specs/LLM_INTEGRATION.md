# LLM_INTEGRATION.md — Seidrum LLM Router Plugin

## Overview

The llm-router is a plugin like any other. It consumes `agent.context.loaded`
events and produces `llm.response` events. Internally, it handles:

- Prompt template rendering
- Context window assembly and token budgeting
- Model selection via routing strategies
- Direct HTTP calls to LLM provider APIs
- Tool call loops
- Cost tracking

There is no LiteLLM or other gateway. The plugin calls provider APIs directly
using reqwest, giving full control over request construction and error handling.

## Provider API Calls

### Anthropic (Claude)

```
POST https://api.anthropic.com/v1/messages
Headers:
  x-api-key: {ANTHROPIC_API_KEY}
  anthropic-version: 2023-06-01
  content-type: application/json

Body:
{
  "model": "claude-sonnet-4-20250514",
  "max_tokens": 4096,
  "system": "system prompt text",
  "messages": [...],
  "tools": [...]
}
```

### OpenAI (GPT)

```
POST https://api.openai.com/v1/chat/completions
Headers:
  Authorization: Bearer {OPENAI_API_KEY}
  content-type: application/json

Body:
{
  "model": "gpt-4o",
  "max_tokens": 4096,
  "messages": [{"role": "system", "content": "..."}, ...],
  "tools": [...]
}
```

### Ollama (local)

```
POST http://{OLLAMA_URL}/api/chat
Body:
{
  "model": "llama3:70b",
  "messages": [...],
  "tools": [...],
  "stream": false
}
```

Each provider has a Rust module in the plugin that normalizes request/response
formats into the common `LlmRequest` / `LlmResponse` types from seidrum-common.

## Routing Strategies

The routing strategy is configured per-agent and determines model selection:

| Strategy    | Behavior                                                 |
|-------------|----------------------------------------------------------|
| best-first  | Use first model in preference list. Fall back on error.  |
| cheap-first | Sort models by cost/token. Use cheapest. Fall back up.   |
| fast-first  | Track rolling average latency per model. Use fastest.    |
| fallback    | Try models in declared order. Next on failure.           |
| specific    | Always use first model. No fallback. Fail if unavailable.|

```rust
enum RoutingStrategy {
    BestFirst,
    CheapFirst,
    FastFirst,
    Fallback,
    Specific,
}

impl RoutingStrategy {
    fn select_model(
        &self,
        preferences: &[String],
        metrics: &ModelMetrics,
    ) -> String {
        match self {
            Self::BestFirst => preferences[0].clone(),
            Self::CheapFirst => {
                let mut sorted = preferences.to_vec();
                sorted.sort_by_key(|m| metrics.cost_per_token(m));
                sorted[0].clone()
            }
            Self::FastFirst => {
                let mut sorted = preferences.to_vec();
                sorted.sort_by_key(|m| metrics.avg_latency_ms(m));
                sorted[0].clone()
            }
            Self::Fallback => preferences[0].clone(), // try in order on failure
            Self::Specific => preferences[0].clone(),
        }
    }
}
```

The plugin maintains a `ModelMetrics` struct with rolling averages of
latency and cost per model, updated after each call.

## Context Window Assembly

When the llm-router receives an `agent.context.loaded` event, it assembles
the context window in this order:

```
┌──────────────────────────────────────────┐
│ 1. System Prompt (Tera template)   ~2000 │
├──────────────────────────────────────────┤
│ 2. Current Facts                   ~3000 │
├──────────────────────────────────────────┤
│ 3. Active Tasks                     ~500 │
├──────────────────────────────────────────┤
│ 4. Similar Content (RAG)           ~4000 │
├──────────────────────────────────────────┤
│ 5. Tool Descriptions               ~2000 │
├──────────────────────────────────────────┤
│ 6. Conversation History            ~5000 │
├──────────────────────────────────────────┤
│ 7. Current User Message              var │
├──────────────────────────────────────────┤
│ 8. Reserved for Response      max_tokens │
└──────────────────────────────────────────┘
```

### Budget Algorithm

```rust
fn assemble_context(config: &LlmConfig, context: &AgentContextLoaded) -> Vec<Message> {
    let total = config.max_context_tokens;
    let reserved = config.max_tokens;
    let available = total - reserved;

    let system = render_prompt(config, context);
    let system_tokens = count_tokens(&system);
    let user_tokens = count_tokens(&context.original_event.text);
    let remaining = available - system_tokens - user_tokens;

    // Proportional allocation
    let facts_budget    = (remaining as f64 * 0.20) as usize;
    let tasks_budget    = (remaining as f64 * 0.05) as usize;
    let rag_budget      = (remaining as f64 * 0.25) as usize;
    let tools_budget    = (remaining as f64 * 0.15) as usize;
    let history_budget  = (remaining as f64 * 0.35) as usize;

    // Fill each section, truncate from least relevant
    // ... assemble messages array
}
```

Token counting uses `tiktoken-rs` with cl100k_base encoding.

## Tool Handling

### Tool Registry

The llm-router queries the kernel's brain for available tools:

1. **Pinned tools** (from agent config) are always included
2. **Dynamic tools** are found via vector search over tool descriptions
   in the brain's tool registry collection

```rust
// Query brain for tools relevant to user's message
let tool_results = nats.request("brain.query.request", BrainQueryRequest {
    query_type: "vector_search".into(),
    collection: Some("tools".into()),
    embedding: Some(message_embedding),
    limit: Some(config.tools.max_dynamic_tools),
    ..Default::default()
}).await?;
```

### Tool Call Loop

```
1. Send messages + tool schemas to LLM
2. If response contains tool_calls:
   a. For each tool call:
      - MCP tool: JSON-RPC request to MCP server
      - Built-in (brain-query): NATS request to kernel
      - CLI tool: execute command (sandboxed)
   b. Append tool results as messages
   c. Send updated messages back to LLM
3. Repeat until response has content (no tool calls)
4. Maximum 10 rounds to prevent infinite loops
```

### Built-in Tools

Registered in brain's tool registry on kernel init:

**brain-query:** Execute a read-only AQL query against the agent's scoped brain.
```json
{
  "name": "brain-query",
  "description": "Query your knowledge graph. Use AQL syntax.",
  "parameters": {
    "type": "object",
    "properties": {
      "description": { "type": "string", "description": "What you want to find" },
      "aql": { "type": "string", "description": "AQL query" }
    },
    "required": ["description", "aql"]
  }
}
```

**web-search:** Search the web (via external MCP server or API).

## Embedding Generation

The content-ingester plugin and graph-context-loader plugin need embeddings.
They call the LLM provider's embedding endpoint directly:

```
POST https://api.openai.com/v1/embeddings
{
  "model": "text-embedding-3-small",
  "input": "text to embed"
}
```

The embedding provider is configurable per-deployment. Could be OpenAI,
Ollama with nomic-embed-text, or any provider with an embeddings endpoint.

## Cost Tracking

The llm-router tracks per-call costs and publishes them in `llm.response`:

```rust
struct TokenUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
    estimated_cost_usd: f64,
}
```

Cost lookup table maintained in the plugin for common models. Updated
manually or via config. Used by cheap-first routing strategy.
