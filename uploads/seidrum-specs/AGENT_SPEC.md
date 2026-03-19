# AGENT_SPEC.md — Seidrum Agent Definition Format

## Overview

An agent in Seidrum is a **declared pipeline** — a wiring diagram of plugins
connected via event types. Agents are defined in YAML files in the `agents/`
directory. The kernel's orchestrator loads, validates, and wires them at startup.

An agent does not process messages itself. It declares which plugins process
which events, in what order, for a given scope and configuration.

## Full Schema

```yaml
# agents/personal-assistant.yaml

agent:
  # Unique identifier. Used in events and logging.
  id: personal-assistant

  # Human-readable name.
  name: Personal Assistant

  # Description (for discovery and documentation).
  description: General-purpose assistant with full knowledge graph access.

  # Whether this agent activates on kernel boot.
  enabled: true

  # --- Scope ---
  scope: scope_root
  additional_scopes:
    - scope_job_search
    - scope_projects

  # --- Pipeline ---
  # The main request/response flow, triggered by incoming events.
  pipeline:
    # What events start this pipeline.
    triggers:
      - channel.telegram.inbound
      - channel.cli.inbound
      - task.completed.*

    # Sequential processing steps.
    steps:
      # Store raw content in brain
      - plugin: content-ingester
        consumes: trigger
        produces: brain.content.stored

      # Load relevant context from knowledge graph
      - plugin: graph-context-loader
        consumes: trigger
        produces: agent.context.loaded
        config:
          graph_depth: 3
          max_facts: 50
          min_confidence: 0.5
          conversation_history_length: 20

      # Call LLM with assembled context
      - plugin: llm-router
        consumes: agent.context.loaded
        produces: llm.response
        config:
          strategy: best-first
          models:
            - claude-sonnet-4-20250514
            - gpt-4o
            - claude-haiku-4-5-20251001
          max_tokens: 4096
          temperature: 0.7
          max_context_tokens: 100000
          prompt: ./prompts/assistant.md
          tools:
            registry_access: true
            pinned: [brain-query, web-search]
            max_dynamic_tools: 5

      # Parse structured actions from LLM output
      - plugin: event-emitter
        consumes: llm.response
        produces:
          - task.created
          - brain.fact.upsert

      # Format response for source channel
      - plugin: response-formatter
        consumes: llm.response
        produces: channel.*.outbound

  # --- Background Processing ---
  # Async plugins triggered by brain events. Don't block user response.
  background:
    - plugin: entity-extractor
      consumes: brain.content.stored
      produces: brain.entity.upsert
      config:
        extraction_model: claude-haiku-4-5-20251001

    - plugin: fact-extractor
      consumes: brain.entity.upserted
      produces: brain.fact.upsert
      config:
        extraction_model: claude-haiku-4-5-20251001

    - plugin: scope-classifier
      consumes: brain.content.stored
      produces: brain.scope.assign
      config:
        classification_model: claude-haiku-4-5-20251001

    - plugin: task-detector
      consumes: llm.response
      produces: task.created
      config:
        detection_model: claude-haiku-4-5-20251001

  # --- Rate Limiting ---
  rate_limit:
    max_calls_per_minute: 30
    max_daily_spend_usd: 5.00
```

## Minimal Agent

```yaml
agent:
  id: simple-bot
  name: Simple Bot
  enabled: true
  scope: scope_root

  pipeline:
    triggers:
      - channel.cli.inbound
    steps:
      - plugin: llm-router
        consumes: trigger
        produces: llm.response
        config:
          strategy: cheap-first
          models: [claude-haiku-4-5-20251001]
          prompt: ./prompts/simple.md
      - plugin: response-formatter
        consumes: llm.response
        produces: channel.cli.outbound
```

## Pipeline Semantics

- **triggers:** NATS subjects that start the pipeline. Supports wildcards.
- **steps:** Sequential. Each step's `consumes` must match the previous step's
  `produces` (or `trigger` for the first step).
- **background:** Async. Triggered by brain events, not by the main pipeline.
  Run after the user gets their response.
- **config:** Per-step configuration passed to the plugin. Each plugin defines
  what config it expects.

The orchestrator validates at startup:
1. All referenced plugins are registered in the system.
2. Event type chains are coherent (outputs match inputs).
3. Scopes exist in the brain.
4. Prompt files exist on disk.

## Prompt Template Variables

Prompts use Tera (Jinja2-compatible) syntax. The llm-router plugin injects:

| Variable                | Type     | Description                            |
|-------------------------|----------|----------------------------------------|
| `user_name`             | String   | From identity scope                    |
| `current_time`          | String   | ISO 8601                               |
| `scope_name`            | String   | Current scope                          |
| `current_facts`         | String   | Relevant facts from graph              |
| `conversation_history`  | String   | Recent messages                        |
| `active_tasks`          | String   | Open tasks for this agent              |
| `available_tools`       | String   | Tool descriptions                      |

Example prompt:

```markdown
You are {{ user_name }}'s personal assistant.

Current time: {{ current_time }}
Context: {{ scope_name }}

## What you know
{{ current_facts }}

## Active tasks
{{ active_tasks }}

## Recent conversation
{{ conversation_history }}

## Instructions
- Be direct and concise.
- If you identify an actionable item, create a task.
- If you learn a new fact, note it in your response.
- Stay within your scope.
```
