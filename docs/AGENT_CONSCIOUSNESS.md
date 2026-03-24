# Agent Consciousness

How agents think, remember, and act autonomously in Seidrum.

## The Problem

Today, agents only exist during a request-response cycle. A user sends a message, the agent thinks, and responds. Between messages, the agent has no awareness, no memory of what it was doing, and no ability to act on its own. This is the equivalent of an assistant who forgets everything the moment you stop talking.

Real intelligence requires:

- **Persistent memory** — structured conversations, not just ingested content
- **Autonomous cognition** — processing events and forming insights without user prompts
- **Delegation** — asking another agent for help without losing conversational context
- **Continuity** — resuming interrupted work, scheduling future actions

## Core Concepts

### Agent Prompt Architecture

Every agent has two layers of prompting:

**1. System prompt (shared, immutable)**

All agents receive the same base system prompt that describes what Seidrum is and how the agent operates within it. This never changes per agent — it provides platform awareness:

```
You are an agent running inside Seidrum, an event-driven AI platform.

## How you work
- You process events from a consciousness stream (user messages, emails, calendar events, other agents, timers)
- You have access to a knowledge graph (the "brain") containing entities, facts, tasks, and conversation history
- You operate within scopes that control what knowledge you can access
- You can delegate work to other agents, schedule future wake-ups, and subscribe to events
- Your thinking is stored as an inner monologue for continuity across sessions

## Built-in capabilities
- subscribe-events: Watch for specific NATS event patterns
- unsubscribe-events: Stop watching for events
- delegate-task: Ask another agent to handle a task
- schedule-wake: Set a timer to resume work later
- send-notification: Proactively message the user on a channel
- get-conversation / list-conversations: Access conversation history

## Important
- You can act autonomously — not every event requires a user response
- When you learn something new, store it as a fact in the brain
- When you identify a task, create it in the brain
- Your scope limits what you can see. Don't assume knowledge outside your scope.

For detailed documentation, see: docs/AGENT_CONSCIOUSNESS.md, docs/PLUGIN_SPEC.md
```

**2. Identity prompt (per-agent, from YAML)**

Each agent has its own prompt template that defines its personality, expertise, and instructions. This is the `prompt` field in the agent YAML, rendered with Tera variables:

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
...
```

The final prompt sent to the LLM is: `system_prompt (shared) + identity_prompt (per-agent) + context (dynamic)`.

### Event Subscriptions

Agents declare which events they care about in their YAML definition:

```yaml
agent:
  id: personal-assistant
  prompt: ./prompts/assistant.md
  tools: [brain-query, execute-code, search-calendar, send-email]
  scope: scope_root
  subscribe:
    - channel.*.inbound        # user messages from any platform
    - task.completed.*         # task completions
    - system.health            # system status changes
  additional_scopes:
    - scope_job_search
    - scope_projects
```

The workflow engine reads the `subscribe` field and routes matching events to `agent.{id}.consciousness`. This is the static, declarative way to wire an agent's awareness.

Agents can also subscribe and unsubscribe at runtime using **built-in capabilities**:

```
tool_call: subscribe-events
arguments: { "subjects": ["deploy.staging.*"], "duration_seconds": 3600 }

tool_call: unsubscribe-events
arguments: { "subjects": ["deploy.staging.*"] }
```

These are core capabilities — not discoverable via the capability registry, but always available to every agent. They allow the agent to dynamically adapt its awareness based on context. For example, when a user says "watch for deployment events," the agent subscribes at runtime without needing a YAML change.

### Conversation Threads

A conversation is a first-class entity — not just ingested content. It is an ordered sequence of messages with structure:

```
Conversation
├── thread_id: unique identifier
├── platform: "telegram" | "cli" | "internal" | "agent"
├── participants: ["user:alice", "agent:personal-assistant"]
├── scope: "scope_projects"
├── messages:
│   ├── { role: "user", content: "Research K8s networking", media: [{ type: "image", url: "..." }], timestamp }
│   ├── { role: "assistant", content: "I'll look into that...", tool_calls: [...], timestamp }
│   ├── { role: "tool", name: "brain-query", result: "...", timestamp }
│   └── { role: "assistant", content: "Here's what I found...", timestamp }
├── metadata:
│   ├── telegram_chat_id: "12345"
│   ├── telegram_thread_id: "678"
│   └── linked_project: "/home/user/projects/app"
└── state: "active" | "archived"
```

Key properties:

- **Multi-modal** — messages contain text, voice transcripts, images, file references
- **Platform-linked** — a Telegram thread maps to a conversation, but the conversation is platform-independent
- **Cross-platform** — the same conversation could continue on CLI after starting on Telegram
- **Threaded** — supports sub-threads (Telegram topics, email threads)
- **Agent-to-agent** — conversations between agents use the same structure

### The Consciousness Loop

Every agent has an internal event stream — its "consciousness." This is a NATS subject (`agent.{id}.consciousness`) where events arrive that the agent should be aware of, even when no user is talking to it.

Events that feed the consciousness:

| Source | Event | Example |
|--------|-------|---------|
| User message | A user sends a message on any channel | "Hey, can you check on that deployment?" |
| Email arrives | The email plugin receives a new message | "Meeting moved to Thursday" |
| Calendar event | A calendar event is approaching | "Standup in 15 minutes" |
| Fact learned | The fact extractor found something new | "Alice now works at Acme (confidence: 0.9)" |
| Task completed | A background task finished | "DNS migration completed successfully" |
| Agent message | Another agent sent a message | "Research agent: here are my findings on K8s networking" |
| Self-wake | A scheduled timer fired | "Time to check back on the deployment" |
| System event | Something changed in the system | "Plugin claude-code was deregistered" |
| Subscribed event | An event matching a runtime subscription | "deploy.staging.completed" |

When an event arrives on the consciousness stream, the agent can:

1. **Respond to user** — call `send-notification` to proactively message the user on their channel
2. **Think silently** — update internal state, form connections, store insights in the brain
3. **Delegate** — call `delegate-task` to ask another agent to handle part of the work
4. **Schedule** — call `schedule-wake` to set a timer and revisit something later
5. **Act** — use tools autonomously (run code, send emails, query the brain)
6. **Subscribe** — call `subscribe-events` to start watching for new event patterns
7. **Ignore** — not everything requires a response

The consciousness loop is what makes an agent feel "alive" — it processes a continuous stream of events, not just user requests.

### How Agents Initiate Conversations

When an agent decides the user should know something — based on an event, a completed task, or an insight — it calls the `send-notification` capability:

```
// Agent's consciousness loop receives: task.completed.task_042
// Agent thinks: "The deployment finished. I should tell the user."

tool_call: send-notification
arguments: {
  "channel": "telegram",
  "chat_id": "12345",
  "text": "Your staging deployment completed successfully. All health checks passing."
}
```

The notification plugin routes the message to the appropriate channel. The user didn't initiate this conversation — the agent did. The message is stored in the conversation thread for context continuity.

For longer interactions, the agent can start a conversation thread and continue updating it:

```
// First notification
send-notification: "Starting your weekly email digest analysis..."

// Agent works autonomously (multiple tool calls)
brain-query → get recent emails
brain-query → cross-reference with tasks
// ... 15 tool calls later ...

// Second notification with results
send-notification: "Here's your weekly digest: [summary of 3 key themes, 2 action items, 1 follow-up needed]"
```

### Inner Monologue

When an agent thinks without producing user-visible output, that thinking is stored as an **inner monologue** — a special conversation thread with `platform: "internal"` and only the agent as a participant.

```
Conversation (inner monologue)
├── thread_id: "consciousness:personal-assistant:2026-03-24"
├── platform: "internal"
├── participants: ["agent:personal-assistant"]
├── messages:
│   ├── { role: "system", content: "New email from Bob about the API migration" }
│   ├── { role: "assistant", content: "This relates to the task I created yesterday. The migration is now done. I should update the task status and let Alice know." }
│   ├── { role: "tool", name: "brain-query", result: "task_001: Migrate API - status: in_progress" }
│   └── { role: "assistant", content: "Updated task to completed. Alice is in the projects scope, so she'll see this next time she asks about tasks." }
```

The inner monologue serves two purposes:

1. **Audit trail** — you can see what the agent was thinking and why it took an action
2. **Context for future conversations** — when a user asks "what have you been up to?", the agent can reference its recent thinking

### Sub-Agent Delegation

An agent can delegate work to another agent using the `delegate-task` built-in capability. This creates a new conversation thread between the two agents:

```
User → Personal Assistant: "Research Kubernetes networking options for our migration"

Personal Assistant thinks:
  "This is a research task. I should delegate to the research agent."

Personal Assistant → (tool: delegate-task) → Research Agent:
  Creates conversation thread: platform: "agent", participants: ["agent:personal-assistant", "agent:research-agent"]
  Message: "Research Kubernetes networking options. We're migrating from Docker Compose. Focus on: CNI plugins, service mesh options, and DNS resolution patterns."

Research Agent works autonomously:
  - Queries the brain for existing knowledge
  - Uses execute-code to test examples
  - Makes multiple LLM calls with tool use
  - Stores findings as facts in the brain

Research Agent → Personal Assistant:
  Message: "Here are my findings: [structured analysis with 3 sections, code examples, and trade-off comparison]"

Personal Assistant → User:
  "I asked the research agent to look into this. Here's what they found: [summary + key recommendations]"
```

The delegation is asynchronous — the personal assistant doesn't block. It can tell the user "I'm looking into that" and notify them when results arrive.

### Self-Wake and Background Work

Agents can schedule themselves to wake up at a future time using the `schedule-wake` built-in capability:

```
tool_call: schedule-wake
arguments: {
  "delay_seconds": 7200,
  "reason": "Check if the deployment completed",
  "context": { "task_id": "task_042", "deployment_url": "https://..." }
}
```

When the timer fires, an event arrives on the agent's consciousness stream:

```json
{
  "type": "self_wake",
  "reason": "Check if the deployment completed",
  "context": { "task_id": "task_042", "deployment_url": "https://..." },
  "scheduled_at": "2026-03-24T10:00:00Z",
  "fired_at": "2026-03-24T12:00:00Z"
}
```

The agent wakes up, checks on the deployment, and either:
- Notifies the user: "The deployment completed successfully"
- Schedules another check: "Still running, I'll check again in 30 minutes"
- Takes action: "The deployment failed, I'm rolling back"

This enables patterns like:

- **Polling** — "Monitor this API endpoint every hour and tell me if latency exceeds 200ms"
- **Follow-up** — "Remind me about this in a week"
- **Background processing** — "Analyze all my emails from last month and summarize the key themes" (runs across many LLM calls over minutes/hours)
- **Proactive notifications** — "Calendar shows a meeting in 15 minutes, but you haven't reviewed the agenda yet"

## Tool Loop Guardrails

Agents can execute long chains of tool calls, but guardrails prevent runaway behavior:

| Guardrail | Default | Description |
|-----------|---------|-------------|
| **Turn limit** | 50 | Maximum tool calls per consciousness event. Configurable per agent. |
| **Budget limit** | Configurable | Maximum token cost per event processing. Prevents expensive runaway loops. |
| **Time limit** | 10 minutes | Maximum wall-clock time per consciousness event. |
| **Loop detection** | 3 repeats | If the agent calls the same tool with identical arguments N times, force stop. |
| **Human-in-the-loop** | After 20 turns | For user-facing conversations, ask "I've been working on this for a while, should I continue?" |

**How the tool loop works:**

1. The LLM produces a response. If it contains tool calls, execute them.
2. Feed the tool results back to the LLM. It produces another response.
3. Repeat until the LLM produces a response with no tool calls (finish_reason: "stop").
4. If any guardrail is hit before natural completion, force a final response.

**Natural termination** is the normal case — the LLM decides it has enough information and produces a final answer. The guardrails are for the degenerate case where the LLM enters a loop or spirals into unnecessary tool calls.

For background work (via consciousness events, delegation, or self-wake), guardrails are relaxed but never removed:

| Context | Turn limit | Time limit | Human-in-the-loop |
|---------|-----------|------------|-------------------|
| User conversation | 50 | 10 min | After 20 turns |
| Consciousness event | 100 | 30 min | No (autonomous) |
| Delegated task | 200 | 60 min | No (report to delegator) |
| Self-wake continuation | 100 | 30 min | No (autonomous) |

## How Scopes Interact with Consciousness

Scopes determine what an agent can see and remember:

- The **personal assistant** operates in `scope_root` — it sees everything
- The **research agent** operates in `scope_projects` — it only sees project-related knowledge
- When the personal assistant delegates to the research agent, the research agent works within its own scope
- Insights from the research agent are stored in `scope_projects` and are visible to both agents (since the personal assistant has `scope_root` which includes all child scopes)

The consciousness loop respects scopes too — an agent only receives events relevant to its scopes. The research agent won't be notified about personal finance emails.

## Real-World Example

Here's a day in the life of an agent with consciousness:

**8:00 AM** — Calendar event: "Team standup at 9:00"
→ Agent stores a reminder in its consciousness
→ At 8:45, self-wake fires: "You have standup in 15 minutes. Open tasks: [3 items]"
→ Agent calls `send-notification` to send a Telegram message to the user

**9:30 AM** — User sends: "Research alternatives to our current logging stack"
→ Agent calls `delegate-task` to hand off to research-agent
→ Tells user via channel response: "I've asked my research agent to look into that. I'll let you know when they're done."

**10:15 AM** — Research agent completes (45 minutes of autonomous work, 47 tool calls)
→ Consciousness event: agent-to-agent message with findings
→ Agent calls `send-notification` to send Telegram message: "The research is done. Here's a summary: [3 options with trade-offs]"

**11:00 AM** — Email arrives from a colleague: "The staging deploy failed"
→ Consciousness event: email content
→ Agent thinks (inner monologue): "This is related to the deployment I've been monitoring. Let me check the logs."
→ Agent uses code-executor to check deployment logs
→ Agent calls `send-notification`: "Heads up — the staging deploy failed. Missing env var. Here's the log line: [...]"
→ Agent calls `subscribe-events` for `deploy.staging.*` to watch for the fix

**12:30 PM** — Subscribed event fires: `deploy.staging.completed`
→ Agent calls `unsubscribe-events` (no longer needed)
→ Agent calls `send-notification`: "The staging deploy has been re-run and succeeded."

**2:00 PM** — Self-wake fires: "Check if the DNS migration TTL has propagated"
→ Agent uses code-executor to run `dig` commands
→ Inner monologue: "TTL has propagated. The migration is complete."
→ Updates task status in the brain
→ Agent calls `send-notification`: "The DNS migration is complete — TTL has fully propagated."

None of this required the user to initiate a conversation. The agent observed events, reasoned about them, and acted — like a real assistant would.

## Architecture

```
                    ┌─────────────────────────────────┐
                    │     Agent Consciousness Loop     │
                    │                                  │
  Events ──────────►  Receive event                   │
  (NATS subject:   │  │                               │
   agent.{id}.     │  ├── Is this relevant? (scope)   │
   consciousness)  │  ├── Load conversation context   │
                    │  ├── Prepend system prompt       │
                    │  ├── Render identity prompt      │
                    │  ├── Call LLM with full context  │
                    │  │   ├── Tool calls? → Execute   │
                    │  │   ├── Guardrail check         │
                    │  │   ├── More tool calls? → Loop │
                    │  │   └── Final response          │
                    │  │                               │
                    │  ├── Store in conversation thread│
                    │  ├── Store in inner monologue    │
                    │  ├── Update brain (facts, tasks) │
                    │  │                               │
                    │  └── Output:                     │
                    │      ├── send-notification       │
                    │      ├── delegate-task            │
                    │      ├── schedule-wake            │
                    │      ├── subscribe-events         │
                    │      └── Silent (no output)      │
                    └─────────────────────────────────┘
```

## Built-in Capabilities

These are always available to every agent. They are not registered via the capability registry — they are core to the agent runtime.

| Capability | Description |
|-----------|-------------|
| `subscribe-events` | Watch for NATS event patterns. Optional `duration_seconds` for auto-expiry. |
| `unsubscribe-events` | Stop watching for events by subject pattern. |
| `delegate-task` | Create a conversation with another agent. Specify target agent ID and message. |
| `schedule-wake` | Set a timer. Specify delay, reason, and arbitrary context JSON. |
| `send-notification` | Proactively message the user on a specific channel and chat. |
| `get-conversation` | Load a conversation thread by ID for context. |
| `list-conversations` | List recent conversations, filterable by participant and platform. |

## Discoverable Capabilities

These are registered by plugins via the capability registry and vary based on which plugins are running:

| Capability | Plugin | Description |
|-----------|--------|-------------|
| `brain-query` | kernel | Query the knowledge graph |
| `execute-code` | code-executor | Run Python/Bash/JS in a sandbox |
| `claude-code` | claude-code | Agentic coding via Claude Code CLI |
| `search-calendar` | calendar | Search calendar events |
| `send-email` | email | Send an email |
| (any registered capability) | (any plugin) | Dynamically discovered at runtime |

## New Collections

| Collection | Purpose |
|-----------|---------|
| `conversations` | Conversation threads with ordered messages, participants, platform links |

The inner monologue uses the same `conversations` collection with `platform: "internal"`.

## Relationship to Existing System

This builds on top of the existing architecture without replacing it:

- **NATS** remains the event bus — consciousness is just another set of subjects
- **ArangoDB** stores conversations alongside entities, facts, and tasks
- **Plugins** expose discoverable capabilities (delegate, schedule-wake are built-in)
- **Scopes** still control access boundaries
- **The workflow engine** routes events to consciousness streams based on agent subscriptions
- **The LLM router** handles the actual thinking (context assembly + LLM calls + tool loops)
- **The system prompt** ensures every agent understands the platform it operates within
