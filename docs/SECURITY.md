# SECURITY.md — Seidrum Security Architecture

## Overview

Seidrum implements a layered security model in the API Gateway plugin.
All external access flows through the gateway, which enforces authentication,
authorization, rate limiting, and audit logging before any request reaches
the kernel or other plugins.

Key dependencies:

| Concern | Crate | Purpose |
|---------|-------|---------|
| JWT | `jsonwebtoken` | HMAC-SHA256 token signing and validation |
| Constant-time comparison | `subtle` | Timing-attack-safe API key validation |
| Password hashing | `argon2` | Argon2id with OWASP-recommended parameters |
| Token IDs | `ulid` | Unique, sortable JTI values for revocation tracking |

---

## Authentication

The API Gateway supports two authentication methods. Both are handled by
`AuthHandler` in `crates/plugins/seidrum-api-gateway/src/auth.rs`.

### 1. API Key Authentication

A shared secret configured via `GATEWAY_API_KEY`. Validated using
constant-time comparison (`subtle::ConstantTimeEq`) to prevent timing
attacks. API key auth always grants the `admin` role with `scope_root`.

Accepted formats:

- **Query parameter:** `?api_key=KEY` (deprecated — logs a warning)
- **Header:** `Authorization: ApiKey KEY`

```
Authorization: ApiKey my-secret-key
```

> **Note:** API key authentication does not associate a `user_id` — it
> represents system-level access, not a specific user.

### 2. JWT Bearer Token Authentication

Tokens are issued via `POST /api/v1/auth/token` and validated on every
request. JWTs carry user identity, role, scopes, and an optional `user_id`
for multi-user support.

```
Authorization: Bearer eyJhbGciOiJIUzI1NiIs...
```

#### JWT Claims

```rust
struct Claims {
    jti: String,            // ULID — unique token ID for revocation
    sub: String,            // Subject (username or "api-user")
    iat: u64,               // Issued-at (unix timestamp)
    exp: u64,               // Expiration (unix timestamp)
    scopes: Vec<String>,    // Scope boundaries (e.g., ["scope_root"])
    role: String,           // "admin" | "user" | "readonly"
    user_id: Option<String>, // User ID for multi-user tokens
}
```

#### Token Lifecycle

1. **Generation:** `POST /api/v1/auth/token` — authenticate with either
   API key or username/password. Returns a signed JWT with configurable TTL
   (default: 86400s / 24h via `GATEWAY_JWT_TTL`).

2. **Validation:** Every authenticated request validates the token signature
   (HMAC-SHA256), checks expiration (no leeway), and checks the in-memory
   revocation list.

3. **Revocation:** `POST /api/v1/auth/revoke` — adds the token's JTI to
   an in-memory `HashSet<String>` protected by `RwLock`. Users can only
   revoke their own tokens; admins can revoke any token.

4. **Persistence:** The revocation list is saved to plugin storage
   (`storage.set` via NATS) every 15 minutes and restored on startup.
   This survives gateway restarts.

5. **Cleanup:** Revocation entries are capped at 10,000. When exceeded,
   the oldest entries are pruned (expired tokens can't pass validation
   anyway, so removing their JTI is safe).

#### Authentication Flow

```
Request
  │
  ├─ Authorization: Bearer <token>
  │   └─ JwtService::validate_token()
  │       ├─ Decode + verify HMAC-SHA256 signature
  │       ├─ Check expiration (validate_exp = true, leeway = 0)
  │       ├─ Check JTI against revocation set
  │       └─ Return Claims → AuthResult
  │
  ├─ Authorization: ApiKey <key>  OR  ?api_key=<key>
  │   └─ validate_api_key() — constant-time via subtle::ConstantTimeEq
  │       └─ Return AuthResult { role: "admin", scopes: ["scope_root"] }
  │
  └─ Neither → 401 Unauthorized
```

---

## Password Hashing (Argon2id)

User passwords are hashed with Argon2id using OWASP-recommended parameters
(recommendation #2 from the [OWASP Password Storage Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html)):

| Parameter | Value | Meaning |
|-----------|-------|---------|
| Algorithm | Argon2id | Hybrid: resistant to both side-channel and GPU attacks |
| Memory (`m`) | 65536 (64 MiB) | Memory cost per hash |
| Iterations (`t`) | 3 | Time cost |
| Parallelism (`p`) | 4 | Degree of parallelism |
| Salt | 16 bytes | Random, generated via `OsRng` per hash |

Implementation in `auth::hash_password()` and `auth::verify_password()`.

**Password policy:** Minimum 12 characters (enforced at registration).

---

## RBAC Model

### Roles

| Role | Description | Capabilities |
|------|-------------|-------------|
| `admin` | Full system access | All endpoints, user management, audit logs, role changes |
| `user` | Standard user | Own profile, own conversations, brain queries within scopes |
| `readonly` | Read-only access | View-only access to permitted resources |

### Scopes

Scopes define knowledge boundaries. Every JWT carries a `scopes` array
that restricts which brain data the user can access. The kernel enforces
scope filtering on every brain query.

- **API key auth** always gets `scope_root` (full access).
- **User auth** gets the scopes assigned to the user record.
- New user registrations default to role `user`.

### Authorization Checks

- **Admin-only endpoints:** User management (`GET /api/v1/users`,
  `PUT /api/v1/users/:id/role`, `DELETE /api/v1/users/:id`), audit log
  access (`GET /api/v1/audit`), admin dashboard.
- **Self-only with admin override:** Profile access (`GET /api/v1/users/me`),
  token revocation (users can only revoke own tokens).

---

## Rate Limiting

Token bucket algorithm with per-subject tracking, implemented in
`crates/plugins/seidrum-api-gateway/src/rate_limiter.rs`.

### Configuration

| Parameter | Env Var | Default | Description |
|-----------|---------|---------|-------------|
| Regular RPM | `GATEWAY_RATE_LIMIT` | 60 | Requests/minute for `user` role |
| Admin RPM | `GATEWAY_RATE_LIMIT_ADMIN` | 300 | Requests/minute for `admin` role |
| Cleanup interval | — | 300s | Stale bucket eviction interval |

### Behavior

- Each subject (identified by JWT `sub` claim or API key identifier) gets
  an independent token bucket.
- Tokens refill continuously at `rpm / 60` tokens per second.
- Per-user RPM overrides are supported via `RateLimiter::set_user_rpm()`.
- When rate-limited, the response includes `Retry-After` header and returns
  `429 Too Many Requests`.
- Successful requests include `X-Rate-Limit-Remaining` header.
- Stale buckets (no activity for 10 minutes) are cleaned up every 5 minutes.
- State is persisted to plugin storage every 5 minutes and restored on startup.

---

## Audit Logging

All authentication events, mutations, and authorization failures are logged
to an in-memory ring buffer and persisted to ArangoDB.

### Audit Entry Structure

```rust
struct AuditEntry {
    timestamp: DateTime<Utc>,
    action: String,       // e.g., "auth.login", "auth.token_revoked", "rate_limit.exceeded"
    subject: String,      // who performed the action
    resource: String,     // what was affected
    method: String,       // HTTP method
    path: String,         // request path
    status: u16,          // HTTP status code
    details: Option<String>,
    user_id: Option<String>, // for multi-tenant audit trails
}
```

### Logged Actions

| Action | Trigger |
|--------|---------|
| `auth.login` | Successful username/password authentication |
| `auth.login_failed` | Failed login (user not found or wrong password) |
| `auth.token_issued` | JWT generated via API key |
| `auth.token_failed` | Token request with invalid API key |
| `auth.token_revoked` | Token revoked via `/auth/revoke` |
| `auth.failed` | Request with no valid authentication |
| `rate_limit.exceeded` | Request rejected by rate limiter |
| `user.registered` | New user account created |

### Storage

- **In-memory:** Capped ring buffer (default 1,000 entries) for fast
  dashboard queries via `GET /api/v1/audit`.
- **ArangoDB:** Every entry is also published to `brain.audit.store`
  (fire-and-forget via NATS) for permanent storage in the `audit_log`
  collection. Historical queries use `brain.audit.query`.

---

## Security Headers and CORS

### CORS

Configured via `CORS_ALLOWED_ORIGINS` environment variable (comma-separated).
Defaults to `http://localhost:3000`.

Allowed methods: `GET`, `POST`, `PUT`, `DELETE`, `OPTIONS`
Allowed headers: `Authorization`, `Content-Type`

### Middleware Stack

The `auth_rate_limit_middleware` (applied to all `/api/v1/*` routes except
`/api/v1/health`) enforces the following pipeline on every request:

```
Request → Authentication → Rate Limiting → Handler → Audit Log
                                                    → X-Rate-Limit-Remaining header
```

### Public Endpoints (No Auth Required)

- `GET /api/v1/health` — Gateway health check
- `GET /dashboard/*` — Static dashboard assets (HTML/CSS/JS)
- `GET /ws` — WebSocket upgrade (auth via `?api_key=` query param)
- `GET /ws/events` — Event stream WebSocket (auth via `?api_key=` query param)

---

## Multi-User Data Isolation

With multi-user support, the system enforces data isolation:

1. **User-scoped tokens:** JWTs include a `user_id` claim that the kernel
   uses to filter brain queries.
2. **Scope boundaries:** Each user's data is associated with scopes. Brain
   queries from user tokens only return data within the user's assigned scopes.
3. **Audit trails:** All audit entries include `user_id` for per-user
   accountability.
4. **Conversation isolation:** Conversation list/get requests include
   `user_id` filtering.
5. **API key isolation:** User-scoped API keys (`api_keys` collection) are
   bound to a specific user and inherit their scopes.

---

## WebSocket Security

### Plugin WebSocket (`/ws`)

Authentication via `?api_key=KEY` query parameter. The key is validated
through `AuthHandler::authenticate()` before the WebSocket upgrade
completes. Unauthenticated connections receive `401 Unauthorized`.

### Event Stream WebSocket (`/ws/events`)

Same authentication as the plugin WebSocket. Supports optional `filter`
(NATS subject pattern) and `correlation_id` query parameters for
scoping the event stream.

### Subject Blocking

External plugins connected via WebSocket are blocked from publishing
to sensitive internal subjects. The `is_subject_allowed()` function in
`ws.rs` rejects any publish to:

| Blocked Prefix | Reason |
|----------------|--------|
| `brain.user.*` | User management (internal) |
| `brain.apikey.*` | API key management (internal) |
| `plugin.register` | Plugin lifecycle (kernel-only) |
| `plugin.deregister` | Plugin lifecycle (kernel-only) |
| `storage.*` | Plugin storage (kernel-only) |
| `capability.register` | Capability lifecycle (kernel-only) |
| `trace.*` | Trace collection (internal) |

---

## Background Maintenance Tasks

The gateway spawns three background tasks at startup:

| Task | Interval | Purpose |
|------|----------|---------|
| Pending request reaper | 5 seconds | Cleans up expired WebSocket request/reply mappings |
| Rate limiter cleanup + save | 5 minutes | Removes stale buckets (>10 min idle), persists state to plugin storage |
| JWT revocation cleanup + save | 15 minutes | Trims revocation list to 10,000 entries, persists to plugin storage |

---

## Constant-Time API Key Comparison

The `validate_api_key()` function uses the `subtle` crate to prevent
timing side-channel attacks:

```rust
use subtle::ConstantTimeEq;

pub fn validate_api_key(provided: &str, expected: &str) -> bool {
    let provided_bytes = provided.as_bytes();
    let expected_bytes = expected.as_bytes();

    if provided_bytes.len() != expected_bytes.len() {
        return false;
    }

    provided_bytes.ct_eq(expected_bytes).into()
}
```

The early return on length mismatch does leak length information, but
this is acceptable because API keys are fixed-length in practice.

---

## Configuration Reference

| Env Var | Required | Default | Description |
|---------|----------|---------|-------------|
| `GATEWAY_API_KEY` | Yes | — | Master API key for system access |
| `GATEWAY_JWT_SECRET` | No | — | HMAC-SHA256 secret for JWT signing (enables JWT auth) |
| `GATEWAY_JWT_TTL` | No | 86400 | JWT token lifetime in seconds |
| `GATEWAY_RATE_LIMIT` | No | 60 | Regular user requests per minute |
| `GATEWAY_RATE_LIMIT_ADMIN` | No | 300 | Admin requests per minute |
| `GATEWAY_LISTEN_ADDR` | No | 0.0.0.0:8080 | Gateway listen address |
| `CORS_ALLOWED_ORIGINS` | No | http://localhost:3000 | Comma-separated allowed origins |
