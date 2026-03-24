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

### Conversation Threads

A conversation is a first-class entity — not just ingested content. It is an ordered sequence of messages with structure:

```
Conversation
├── thread_id: unique identifier
├── platform: "telegram" | "cli" | "internal" | "agent"
├── participants: ["user:alice", "agent:personal-assistant"]
├── scope: "scope_projects"
├── messages:
│   ├── { role: "user", content: "Research K8s networking", media: [], timestamp }
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

When an event arrives on the consciousness stream, the agent can:

1. **Respond to user** — send a message back on the originating channel
2. **Think silently** — update internal state, form connections, store insights in the brain
3. **Delegate** — ask another agent to handle part of the work
4. **Schedule** — set a timer to revisit something later
5. **Act** — use tools autonomously (run code, send emails, query the brain)
6. **Ignore** — not everything requires a response

The consciousness loop is what makes an agent feel "alive" — it processes a continuous stream of events, not just user requests.

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

An agent can delegate work to another agent using a tool call. This creates a new conversation thread between the two agents:

```
User → Personal Assistant: "Research Kubernetes networking options for our migration"

Personal Assistant thinks:
  "This is a research task. I should delegate to the research agent."

Personal Assistant → (tool: delegate_task) → Research Agent:
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

Agents can schedule themselves to wake up at a future time using a `schedule-wake` capability:

```
// Agent schedules a wake-up
tool_call: schedule-wake
arguments: {
  "delay_seconds": 7200,  // 2 hours
  "reason": "Check if the deployment completed",
  "context": { "task_id": "task_042", "deployment_url": "https://..." }
}
```

When the timer fires, an event arrives on the agent's consciousness stream:

```
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

### Infinite Tool Turns

Agents are not limited to a fixed number of tool calls per conversation turn. They can execute arbitrarily long chains of tool use:

```
User: "Refactor the authentication module to use JWT instead of sessions"

Agent turn 1: brain-query → finds existing auth code and related facts
Agent turn 2: execute-code → reads current auth module files
Agent turn 3: claude-code → makes the initial refactoring changes
Agent turn 4: execute-code → runs the test suite
Agent turn 5: claude-code → fixes failing tests
Agent turn 6: execute-code → runs tests again (all pass)
Agent turn 7: execute-code → runs clippy
Agent turn 8: claude-code → fixes clippy warnings
Agent turn 9: brain-query → stores the refactoring as a completed task
Agent turn 10: responds to user with summary of changes
```

The LLM router manages the tool loop — it keeps calling the LLM until it produces a final response (no more tool calls) or hits a configurable maximum. For background work (via consciousness or delegation), there is effectively no limit — the agent keeps going until the work is done or the budget is exhausted.

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
→ Agent sends a proactive Telegram message to the user

**9:30 AM** — User sends: "Research alternatives to our current logging stack"
→ Agent delegates to research-agent
→ Tells user: "I've asked my research agent to look into that. I'll let you know when they're done."

**10:15 AM** — Research agent completes (45 minutes of autonomous work)
→ Consciousness event: agent-to-agent message with findings
→ Agent sends Telegram message: "The research is done. Here's a summary: [3 options with trade-offs]"

**11:00 AM** — Email arrives from a colleague: "The staging deploy failed"
→ Consciousness event: email content
→ Agent thinks (inner monologue): "This is related to the deployment I've been monitoring. Let me check the logs."
→ Agent uses code-executor to check deployment logs
→ Sends Telegram: "Heads up — the staging deploy failed. It looks like a missing env var. Here's the relevant log line: [...]"

**2:00 PM** — Self-wake fires: "Check if the DNS migration TTL has propagated"
→ Agent uses code-executor to run `dig` commands
→ Inner monologue: "TTL has propagated. The migration is complete."
→ Updates task status in the brain
→ Sends Telegram: "The DNS migration is complete — TTL has fully propagated."

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
                    │  ├── Call LLM with full context  │
                    │  │   ├── Tool calls? → Execute   │
                    │  │   ├── More tool calls? → Loop │
                    │  │   └── Final response          │
                    │  │                               │
                    │  ├── Store in conversation thread│
                    │  ├── Store in inner monologue    │
                    │  ├── Update brain (facts, tasks) │
                    │  │                               │
                    │  └── Output:                     │
                    │      ├── Reply to user channel   │
                    │      ├── Delegate to sub-agent   │
                    │      ├── Schedule self-wake      │
                    │      └── Silent (no output)      │
                    └─────────────────────────────────┘
```

## New Capabilities Required

| Capability | Plugin | Description |
|-----------|--------|-------------|
| `delegate-task` | kernel or new plugin | Create a conversation between agents, invoke the target agent |
| `schedule-wake` | scheduler plugin | Set a timer that fires an event on the agent's consciousness |
| `send-notification` | notification plugin | Proactively message the user on a channel |
| `get-conversation` | kernel/brain | Load a conversation thread by ID |
| `list-conversations` | kernel/brain | List recent conversations for context |

## New Collections

| Collection | Purpose |
|-----------|---------|
| `conversations` | Conversation threads with ordered messages, participants, platform links |
| `consciousness_log` | Inner monologue entries (special conversation type) |

## Relationship to Existing System

This builds on top of the existing architecture without replacing it:

- **NATS** remains the event bus — consciousness is just another set of subjects
- **ArangoDB** stores conversations alongside entities, facts, and tasks
- **Plugins** expose the new capabilities (delegate, schedule-wake)
- **Scopes** still control access boundaries
- **The workflow engine** triggers the consciousness loop (a new workflow type)
- **The LLM router** handles the actual thinking (context assembly + LLM calls + tool loops)
