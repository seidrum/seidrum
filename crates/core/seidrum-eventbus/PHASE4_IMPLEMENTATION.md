# Phase 4: Transport Servers + Remote Delivery Channels

## Overview

Phase 4 adds network transport capabilities to seidrum-eventbus, allowing remote clients to interact with the event bus via WebSocket and HTTP protocols. This enables distributed event publishing, subscription, and request/reply patterns.

## Key Components

### 1. Delivery Channels

#### Custom Channel Registry (`src/delivery/registry.rs`)
- `ChannelRegistry`: Thread-safe registry for pluggable delivery channels
- Methods:
  - `register(type_name, channel)`: Register a delivery channel
  - `get(type_name)`: Look up a registered channel
  - `unregister(type_name)`: Unregister a channel
  - `list_types()`: List all registered channel types

#### WebSocket Delivery Channel (`src/delivery/websocket.rs`)
- `WebSocketChannel`: Implements `DeliveryChannel` trait
- Delivers events by writing JSON frames to WebSocket connections
- Message format:
  ```json
  {
    "op": "event",
    "subject": "test.topic",
    "payload": "base64_encoded_bytes"
  }
  ```
- Features:
  - Fast in-memory delivery
  - Connection health tracking
  - Automatic error handling

#### Webhook Delivery Channel (`src/delivery/webhook.rs`)
- `WebhookChannel`: HTTP POST delivery with retry support
- Configuration:
  - Target URL
  - Custom headers
  - Configurable retry policy
- Retry logic:
  - Exponential backoff with jitter
  - Configurable max attempts
  - Max backoff capping
- Health checks via GET requests

#### Retry Task (`src/delivery/retry.rs`)
- `RetryTask`: Background task for failed delivery retry
- Features:
  - Periodic polling of retryable deliveries
  - Exponential backoff with jitter
  - Configurable max attempts
  - Integration with EventStore

### 2. Transport Servers

#### WebSocket Transport (`src/transport/ws.rs`)
- `WebSocketServer`: Listens for WebSocket connections
- Client Operations (JSON protocol):
  - `publish`: Publish an event to a subject
  - `subscribe`: Subscribe to a pattern
  - `unsubscribe`: Unsubscribe from a subscription
  - `request`: Make a request/reply call
  - `reply`: Reply to a request
- Server Messages:
  - `event`: Delivered event
  - `reply_result`: Reply to a request
  - `error`: Error message
- Features:
  - Per-connection subscription tracking
  - Base64 payload encoding
  - Automatic cleanup on disconnect

Example client connection:
```json
{"op": "publish", "subject": "test.topic", "payload": "aGVsbG8="}
{"op": "subscribe", "pattern": "test.*", "opts": {"priority": 10}}
{"op": "request", "subject": "api.command", "payload": "...", "timeout_ms": 5000}
```

#### HTTP Transport (`src/transport/http.rs`)
- `HttpServer`: Axum-based REST API server
- Endpoints:
  - `POST /publish` - Publish an event
  - `POST /request` - Make a request/reply
  - `POST /subscribe` - Create webhook subscription
  - `DELETE /subscribe/:id` - Remove subscription
  - `GET /events/:seq` - Get event by sequence (stub)
  - `GET /health` - Health check
  - `GET /metrics` - Bus metrics
- Features:
  - JSON request/response bodies
  - Base64 payload encoding
  - Error handling with HTTP status codes
  - CORS support
  - API key authentication (extensible)

Example requests:
```bash
# Publish
curl -X POST http://localhost:8000/publish \
  -H "Content-Type: application/json" \
  -d '{"subject": "test.topic", "payload": "aGVsbG8="}'

# Subscribe to webhook
curl -X POST http://localhost:8000/subscribe \
  -H "Content-Type: application/json" \
  -d '{"pattern": "test.*", "url": "https://example.com/webhook", "priority": 10}'

# Make request
curl -X POST http://localhost:8000/request \
  -H "Content-Type: application/json" \
  -d '{"subject": "api.cmd", "payload": "...", "timeout_ms": 5000}'
```

### 3. Builder Integration

Updated `EventBusBuilder` with transport configuration:
```rust
let bus = EventBusBuilder::new()
    .storage(store)
    .with_websocket("127.0.0.1:9000".parse().unwrap())
    .with_http("127.0.0.1:8000".parse().unwrap())
    .build()
    .await?;
```

Both servers are spawned as background tasks and run for the lifetime of the bus.

## Configuration

### Dependencies Added
- `axum = "0.8"` - HTTP server framework
- `tokio-tungstenite = "0.26"` - WebSocket support
- `futures-util = "0.3"` - Async utilities
- `reqwest = { version = "0.12", features = ["json"] }` - HTTP client
- `tower = "0.5"` - Middleware framework
- `tower-http = { version = "0.6", features = ["cors"] }` - HTTP utilities
- `rand = "0.9"` - Random number generation (jitter)
- `hyper = "1"` - HTTP protocol
- `base64 = "0.22"` - Base64 encoding

### Environment Variables
None required. Configuration via builder API.

## Architecture Decisions

### Protocol Design
- **Base64 Encoding**: Payloads are base64-encoded for safe JSON transport
- **Async Operations**: All I/O is async using Tokio
- **Error Handling**: Delivery failures trigger retries, not immediate failures

### Retry Strategy
- Exponential backoff: 2^attempt * initial_backoff_ms
- Jitter: ±25% to prevent thundering herd
- Default: max 5 attempts, 100ms initial, 30s max

### Scalability
- WebSocket: Supports many concurrent connections
- HTTP: RESTful, load-balanceable
- Webhook delivery: Non-blocking HTTP client

## Testing

Integration tests in `tests/phase4_transport.rs`:
- Channel registry operations
- WebSocket delivery mechanics
- Webhook configuration parsing
- Retry backoff calculations
- Builder with transport config

## Security Considerations

### Current Implementation
- No authentication by default on transports
- WebSocket: No connection authentication
- HTTP: Middleware structure supports auth integration

### Recommended Enhancements (Future)
- API key validation
- TLS/HTTPS for HTTP transport
- WSS (WebSocket Secure) support
- Rate limiting
- Request signing

## Future Phases

- **Phase 5a**: NATS Bridge - Connect to NATS JetStream
- **Phase 5b**: gRPC Transport - High-performance gRPC transport
- **Phase 6**: Connection Pools - Long-lived connection management
- **Phase 7**: Compression - Payload compression for large events

## Files Added

```
src/
├── delivery/
│   ├── registry.rs          # Channel registry
│   ├── websocket.rs         # WebSocket delivery channel
│   ├── webhook.rs           # Webhook delivery channel
│   ├── retry.rs             # Retry task
│   └── mod.rs               # Updated with new modules
├── transport/
│   ├── mod.rs               # Transport module root
│   ├── ws.rs                # WebSocket server
│   └── http.rs              # HTTP server
└── lib.rs                   # Updated with transport exports

Cargo.toml                    # Updated with transport dependencies
tests/phase4_transport.rs     # Integration tests
```

## Files Modified

- `src/lib.rs` - Added transport module and new re-exports
- `src/delivery/mod.rs` - Added registry, websocket, webhook, retry modules
- `src/builder.rs` - Added transport configuration methods
- `Cargo.toml` - Added transport dependencies

## Compilation

All code compiles with `cargo build -p seidrum-eventbus` (subject to target directory permissions).

The implementation follows existing code patterns:
- Async-trait for trait objects
- Tracing for logging
- Serde for serialization
- Error handling with `thiserror`
- Comprehensive doc comments
