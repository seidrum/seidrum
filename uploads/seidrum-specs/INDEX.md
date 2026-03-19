# Seidrum — Specification Index

> "The space where hidden connections are seen"
> From Old Norse: seidr (seeing hidden connections) + rum (space/room)

These documents form the complete specification for building Seidrum.
Each document is self-contained and referenced by name.

## Documents

| #  | Document                  | Purpose                              |
|----|---------------------------|--------------------------------------|
| 1  | PROJECT.md                | Vision, principles, glossary         |
| 2  | ARCHITECTURE.md           | System architecture + component map  |
| 3  | BRAIN_SCHEMA.md           | ArangoDB collections, edges, indexes |
| 4  | TECH_STACK.md             | Rust crates, infra, Docker Compose   |
| 5  | EVENT_CATALOG.md          | Every event type, payload schemas    |
| 6  | AGENT_SPEC.md             | Agent YAML format, lifecycle, config |
| 7  | PLUGIN_SPEC.md            | Plugin interface, registry, built-ins|
| 8  | LLM_INTEGRATION.md        | LLM plugin, context assembly, tools  |
| 9  | BUILD_PLAN.md             | Ordered task list for Claude Code    |

## How to Use with Claude Code

For each task in BUILD_PLAN.md, start a session and feed:
1. BUILD_PLAN.md (current task and dependencies)
2. PROJECT.md (vocabulary and principles)
3. Whichever spec documents the task references

The documents are designed so that Claude Code never has to guess at a
type signature, event format, or collection name.
