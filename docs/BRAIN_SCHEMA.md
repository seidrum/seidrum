# BRAIN_SCHEMA.md — Seidrum Knowledge Graph

## Design for ArangoDB Multi-Model Database

> The Brain is the core knowledge layer of Seidrum. It stores everything
> the system knows about the user, their world, and the relationships
> between entities. It is scoped, temporal, and queryable via both graph
> traversal and vector similarity.
>
> Only the kernel has direct ArangoDB access. All plugins interact with
> the brain through NATS request/reply events.

---

## Design Principles

1. **Everything is an event first.** Content enters via NATS events.
   Plugins request the kernel to store and extract knowledge.
2. **Scopes are boundaries, not silos.** A scope defines a context
   (project, life area). Entities can exist in multiple scopes. The kernel
   enforces scopes on every query.
3. **Facts decay. Content doesn't.** Raw content is immutable. Facts
   extracted from content have temporal validity and confidence that
   changes over time.
4. **Graph answers "how things relate." Vectors answer "what's similar."**
   Both queryable in a single AQL query via ArangoDB's multi-model engine.
5. **Provenance is mandatory.** Every fact traces back to the content it
   was extracted from. Nothing exists without attribution.

---

## Vertex Collections

### `entities`

Core nodes: people, organizations, projects, tools, locations, concepts.

```json
{
  "_key": "entity_alice",
  "type": "person",
  "name": "Alice Smith",
  "aliases": ["alice-s"],
  "properties": {
    "email": "alice@example.com",
    "location": "Berlin, Germany",
    "role": "Software Engineer"
  },
  "embedding": [0.12, -0.34, ...],
  "created_at": "2024-01-15T10:00:00Z",
  "updated_at": "2026-03-08T12:00:00Z",
  "source": "manual"
}
```

Entity types: `person`, `organization`, `project`, `product`, `concept`,
`location`, `tool`, `channel`.

### `content`

Raw ingested content. Messages, emails, documents, files. Immutable.

```json
{
  "_key": "content_msg_20260308_001",
  "type": "message",
  "channel": "telegram",
  "channel_id": "chat_12345",
  "raw_text": "Hey Alice, the Acme API migration is done...",
  "summary": "Acme API infrastructure migration completed",
  "language": "en",
  "sentiment": 0.7,
  "embedding": [0.45, -0.12, ...],
  "timestamp": "2026-03-08T09:15:00Z",
  "ingested_at": "2026-03-08T09:15:02Z",
  "metadata": {
    "from": "entity_bob",
    "to": "entity_alice",
    "thread_id": "thread_api_migration",
    "has_attachments": false
  }
}
```

### `facts`

Temporal knowledge claims with confidence, provenance, and decay.

```json
{
  "_key": "fact_001",
  "subject": "entity_alex",
  "predicate": "works_at",
  "object": "entity_acme_corp",
  "value": null,
  "valid_from": "2022-03-01T00:00:00Z",
  "valid_to": "2025-12-31T00:00:00Z",
  "confidence": 0.95,
  "source_content": "content_email_20250101_001",
  "extraction_method": "llm",
  "superseded_by": "fact_042",
  "last_reinforced": "2026-02-15T00:00:00Z",
  "reinforcement_count": 3,
  "created_at": "2022-03-15T00:00:00Z"
}
```

**Temporal query patterns:**
- **Current truth:** `valid_to == null AND superseded_by == null`
- **Truth at time T:** `valid_from <= T AND (valid_to >= T OR valid_to == null)`
- **Confidence decay:** Unreinforced facts lose confidence via decay function.

**Common predicates:** `works_at`, `owns`, `knows`, `located_in`, `uses_tool`,
`has_role`, `interested_in`, `depends_on`, `status_is`, `prefers`.

### `scopes`

Context boundaries for knowledge isolation.

```json
{
  "_key": "scope_job_search",
  "name": "Job Search",
  "type": "life_area",
  "description": "Active job search for VP/Head of Engineering roles",
  "parent_scope": "scope_career",
  "access_rules": {
    "cross_scope_read": ["scope_career", "scope_identity"],
    "cross_scope_write": []
  },
  "status": "active",
  "created_at": "2025-06-01T00:00:00Z"
}
```

**Scope hierarchy:**
```
scope_root (identity — always accessible)
├── scope_career
│   ├── scope_job_search
│   └── scope_acme
├── scope_projects
│   ├── scope_webapp_alpha
│   └── scope_seidrum
├── scope_personal
└── scope_finance
```

### `tasks`

Persistent task objects that survive agent sessions.

```json
{
  "_key": "task_001",
  "title": "Migrate Acme DNS to new provider",
  "status": "in_progress",
  "priority": "high",
  "assigned_agent": "personal-assistant",
  "created_by": "entity_alice",
  "created_from": "content_msg_20260308_001",
  "due_date": "2026-03-10T00:00:00Z",
  "completion_event": "task.completed.task_001",
  "callback_channel": "telegram",
  "subtasks": ["task_001a", "task_001b"],
  "context": {
    "scope": "scope_acme_corp",
    "related_entities": ["entity_acme_corp", "entity_cloudhost"]
  },
  "created_at": "2026-03-08T09:20:00Z",
  "updated_at": "2026-03-08T14:00:00Z"
}
```

### `event_types`

Registry of all event types in the system.

```json
{
  "_key": "channel_telegram_inbound",
  "subject": "channel.telegram.inbound",
  "schema": { "type": "object", "properties": { ... } },
  "description": "Incoming message from Telegram",
  "producers": ["telegram"],
  "consumers": ["content-ingester", "graph-context-loader"]
}
```

### `users`

User accounts for multi-user support. Passwords are stored as Argon2id hashes.

```json
{
  "_key": "user_abc123def456",
  "username": "alice",
  "password_hash": "$argon2id$v=19$m=65536,t=3,p=4$...",
  "email": "alice@example.com",
  "display_name": "Alice Smith",
  "role": "user",
  "status": "active",
  "scopes": ["scope_root"],
  "created_at": "2026-03-15T10:00:00Z",
  "updated_at": "2026-03-15T10:00:00Z"
}
```

**Fields:**
- `_key`: `user_` prefix + 12-char lowercase ULID
- `username`: Unique, used for login (unique persistent index)
- `password_hash`: Argon2id hash (m=65536, t=3, p=4)
- `email`: Optional, unique sparse index
- `display_name`: Optional display name
- `role`: `"admin"` | `"user"` | `"readonly"`
- `status`: `"active"` | `"suspended"` | `"deleted"` (soft-delete)
- `scopes`: Array of scope keys this user can access
- `created_at` / `updated_at`: ISO 8601 timestamps

**Indexes:** `username` (unique), `email` (unique, sparse), `role`, `status`

### `audit_log`

Immutable audit trail for security-relevant events. Written by the kernel
via `brain.audit.store` (fire-and-forget from the API Gateway).

```json
{
  "_key": "audit_01JQRS...",
  "timestamp": "2026-03-15T10:05:00Z",
  "action": "auth.login",
  "subject": "alice",
  "resource": "auth_token",
  "method": "POST",
  "path": "/api/v1/auth/token",
  "status": 200,
  "details": null,
  "user_id": "user_abc123def456"
}
```

**Fields:**
- `action`: Event type (e.g., `auth.login`, `auth.login_failed`, `auth.token_revoked`, `rate_limit.exceeded`, `user.registered`)
- `subject`: Who performed the action (username or "api-key")
- `resource`: What was affected
- `method` / `path`: HTTP method and request path
- `status`: HTTP response status code
- `user_id`: Optional user ID for multi-tenant audit trails

**Indexes:** `timestamp`, `action`, `user_id` (sparse), `subject`

### `api_keys`

User-scoped API keys for programmatic access. The actual key is never stored;
only a hash is persisted.

```json
{
  "_key": "apikey_01JQRS...",
  "user_id": "user_abc123def456",
  "name": "CI Pipeline Key",
  "key_hash": "sha256:a1b2c3...",
  "scopes": ["scope_root"],
  "created_at": "2026-03-15T10:10:00Z",
  "expires_at": "2026-06-15T10:10:00Z",
  "last_used": "2026-03-16T08:00:00Z",
  "revoked": false
}
```

**Fields:**
- `user_id`: Owning user's key
- `name`: Human-readable label
- `key_hash`: SHA-256 hash of the API key (unique index)
- `scopes`: Scope boundaries inherited from the user
- `expires_at`: Optional expiration timestamp
- `last_used`: Last successful authentication timestamp
- `revoked`: Whether the key has been revoked

**Indexes:** `user_id`, `key_hash` (unique)

---

## Edge Collections

### `relates_to` — Entity-to-entity relationships

```json
{
  "_from": "entities/entity_alice",
  "_to": "entities/entity_acme",
  "type": "works_at",
  "properties": { "role": "engineer" },
  "valid_from": "2024-01-01T00:00:00Z",
  "valid_to": null,
  "confidence": 1.0,
  "source_content": "content_registration_doc",
  "scopes": ["scope_career", "scope_acme"]
}
```

### `mentions` — Content references entities

```json
{
  "_from": "content/content_msg_20260308_001",
  "_to": "entities/entity_acme",
  "context_snippet": "...the Acme API migration...",
  "mention_type": "direct",
  "sentiment_toward": 0.3,
  "scopes": ["scope_acme"]
}
```

### `scoped_to` — Assigns items to scopes

```json
{
  "_from": "entities/entity_some_tool",
  "_to": "scopes/scope_projects",
  "relevance": 0.9,
  "added_at": "2026-02-17T00:00:00Z",
  "added_by": "system"
}
```

### `derived_from` — Provenance chain

```json
{
  "_from": "facts/fact_001",
  "_to": "content/content_email_20250101_001",
  "extraction_method": "llm",
  "extraction_confidence": 0.92,
  "extracted_at": "2025-01-02T00:00:00Z",
  "extractor_model": "claude-sonnet-4"
}
```

### `supersedes` — Fact versioning

```json
{
  "_from": "facts/fact_042",
  "_to": "facts/fact_001",
  "reason": "User confirmed project shutdown complete",
  "superseded_at": "2026-01-15T00:00:00Z"
}
```

### `participated_in` — Entity involvement in content

```json
{
  "_from": "entities/entity_alice",
  "_to": "content/content_msg_20260308_001",
  "role": "recipient",
  "channel": "telegram",
  "timestamp": "2026-03-08T09:15:00Z"
}
```

### `task_relates` — Task connections

```json
{
  "_from": "tasks/task_001",
  "_to": "entities/entity_cloudhost",
  "relation": "involves"
}
```

---

## Named Graph

```json
{
  "name": "brain",
  "edgeDefinitions": [
    { "collection": "relates_to", "from": ["entities"], "to": ["entities"] },
    { "collection": "mentions", "from": ["content"], "to": ["entities"] },
    { "collection": "scoped_to", "from": ["entities", "content", "facts", "tasks", "conversations"], "to": ["scopes", "users"] },
    { "collection": "derived_from", "from": ["facts", "content"], "to": ["content"] },
    { "collection": "supersedes", "from": ["facts"], "to": ["facts"] },
    { "collection": "participated_in", "from": ["entities"], "to": ["content"] },
    { "collection": "task_relates", "from": ["tasks"], "to": ["entities", "content", "tasks"] }
  ]
}
```

---

## Vector Indexes

```js
db.entities.ensureIndex({
  type: "vector", fields: ["embedding"],
  params: { dimension: 1536, metric: "cosine", nLists: 100 }
});

db.content.ensureIndex({
  type: "vector", fields: ["embedding"],
  params: { dimension: 1536, metric: "cosine", nLists: 200 }
});

// Full-text search via ArangoSearch
db._createView("content_search", "arangosearch", {
  links: {
    content: { fields: { raw_text: { analyzers: ["text_en"] }, summary: { analyzers: ["text_en"] } } },
    entities: { fields: { name: { analyzers: ["text_en", "identity"] } } }
  }
});
```

---

## Key Query Patterns

### Scoped Context Retrieval

```aql
LET scope_entities = (
  FOR v, e IN 1..1 INBOUND @scope_key scoped_to
    FILTER IS_SAME_COLLECTION("entities", v)
    RETURN v
)
FOR entity IN scope_entities
  LET current_facts = (
    FOR fact IN facts
      FILTER fact.subject == entity._id
         AND fact.valid_to == null AND fact.superseded_by == null
         AND fact.confidence > 0.5
      SORT fact.confidence DESC
      RETURN fact
  )
  RETURN { entity, facts: current_facts }
```

### Hybrid GraphRAG Query

```aql
LET similar_content = (
  FOR doc IN content
    LET score = COSINE_SIMILARITY(doc.embedding, @query_embedding)
    FILTER score > 0.7
    SORT score DESC LIMIT 10
    RETURN { doc, score }
)
LET mentioned_entities = (
  FOR item IN similar_content
    FOR v, e IN 1..1 OUTBOUND item.doc mentions
      RETURN DISTINCT v
)
LET expanded = (
  FOR entity IN mentioned_entities
    FOR v, e IN 1..2 ANY entity relates_to
      FILTER @current_scope IN e.scopes OR e.scopes == null
      RETURN { entity: v, relation: e }
)
RETURN { similar_content, mentioned_entities, expanded_context: expanded }
```

### Confidence Decay (scheduled by kernel)

```aql
LET decay_threshold = DATE_SUBTRACT(DATE_NOW(), 90, "day")
FOR fact IN facts
  FILTER fact.valid_to == null AND fact.superseded_by == null
     AND fact.last_reinforced < decay_threshold AND fact.confidence > 0.1
  LET days_since = DATE_DIFF(fact.last_reinforced, DATE_NOW(), "day")
  LET new_confidence = MAX(0.1, fact.confidence - (days_since * 0.005))
  UPDATE fact WITH { confidence: new_confidence } IN facts
```

---

## Ingestion Flow (via plugins + kernel)

```
NATS event (channel.telegram.inbound)
  ├─→ Content Ingester Plugin → brain.content.store → Kernel stores + embeds
  │                                                  → brain.content.stored
  ├─→ Entity Extractor Plugin (async) → brain.entity.upsert → Kernel stores
  │                                                          → brain.entity.upserted
  ├─→ Fact Extractor Plugin (async) → brain.fact.upsert → Kernel stores
  │                                                      → brain.fact.upserted
  └─→ Scope Classifier Plugin (async) → brain.scope.assign → Kernel stores
                                                            → brain.scope.assigned
```

---

## Storage Estimates (single power user, 2 years)

| Collection    | Records   | Total     |
|---------------|-----------|-----------|
| entities      | 5,000     | 10 MB     |
| content       | 500,000   | 1.5 GB    |
| facts         | 50,000    | 25 MB     |
| scopes        | 50        | 25 KB     |
| tasks         | 2,000     | 2 MB      |
| edges (all)   | 1,170,000 | 340 MB    |
| embeddings    | 555,000   | 3.3 GB    |
| **Total**     |           | **~5.2 GB** |

| users         | 1,000     | 1 MB      |
| audit_log     | 100,000   | 50 MB     |
| api_keys      | 5,000     | 2 MB      |

Fits on a single ArangoDB instance. Runs on a 4GB VPS.
