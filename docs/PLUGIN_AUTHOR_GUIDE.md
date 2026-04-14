# Plugin Author Guide

How to write plugins for Seidrum using the eventbus.

## Quick start

Every Seidrum plugin follows the same pattern:

```rust
use seidrum_common::bus_client::BusClient;
use seidrum_common::events::{EventEnvelope, PluginRegister};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Connect to the kernel's eventbus (retries automatically)
    let bus = BusClient::connect("ws://127.0.0.1:9000", "my-plugin").await?;

    // 2. Register with the kernel
    let register = PluginRegister {
        id: "my-plugin".into(),
        name: "My Plugin".into(),
        version: "0.1.0".into(),
        consumes: vec!["channel.*.inbound".into()],
        produces: vec!["my-plugin.processed".into()],
        health_subject: "plugin.my-plugin.health".into(),
        consumed_event_types: vec![],
        produced_event_types: vec![],
        config_schema: None,
    };
    bus.publish_envelope("plugin.register", None, None, &register).await?;

    // 3. Subscribe to events
    let mut sub = bus.subscribe("channel.*.inbound").await?;

    // 4. Process events
    while let Some(msg) = sub.next().await {
        let envelope: EventEnvelope = serde_json::from_slice(&msg.payload)?;
        // ... process the event ...
        bus.publish_envelope("my-plugin.processed", envelope.correlation_id, envelope.scope, &result).await?;
    }

    Ok(())
}
```

## Connection

`BusClient::connect(url, source)` connects to the kernel's WebSocket transport.

- **URL**: `ws://127.0.0.1:9000` for local dev, `ws://kernel:9000` in Docker
- **Source**: your plugin's identifier, stamped onto every `EventEnvelope` you publish
- **Retry**: automatically retries up to 20 times with exponential backoff if the kernel isn't up yet

The `BUS_URL` environment variable overrides the URL:
```bash
BUS_URL=ws://kernel:9000 cargo run -p my-plugin
```

## Subscribing

```rust
let mut sub = bus.subscribe("channel.telegram.inbound").await?;
while let Some(msg) = sub.next().await {
    println!("subject: {}, payload: {} bytes", msg.subject, msg.payload.len());
}
```

**Wildcards:**
- `*` matches one token: `channel.*.inbound` matches `channel.telegram.inbound`
- `>` matches one or more tokens at the tail: `channel.>` matches `channel.telegram.inbound` and `channel.cli.outbound`

## Publishing

```rust
// Typed (auto-serializes via serde_json)
bus.publish("my.subject", &my_struct).await?;

// Raw bytes (when you've already serialized)
bus.publish_bytes("my.subject", serialized_bytes).await?;

// With EventEnvelope wrapping (standard for inter-plugin events)
bus.publish_envelope("my.subject", correlation_id, scope, &payload).await?;
```

## Request / Reply

```rust
// Typed request → typed response
let response: MyResponse = bus.request("brain.query", &my_request).await?;

// Raw bytes
let response_bytes = bus.request_bytes("brain.query", request_bytes).await?;
```

The kernel's brain service, tool registry, and other kernel services all accept request/reply on their documented subjects (see `docs/EVENT_CATALOG.md`).

## Writing a sync interceptor plugin

Sync interceptors run **before** async subscribers in the dispatch chain. They can inspect, modify, or drop events.

To register a sync interceptor, your plugin connects via WebSocket and sends:

```json
{"op": "register_interceptor", "pattern": "channel.*.inbound", "priority": 100}
```

The server responds with:
```json
{"op": "interceptor_registered", "id": "sub-abc123"}
```

Then for each matching event, the server sends:
```json
{"op": "intercept", "request_id": "req-1", "subject": "channel.telegram.inbound", "payload": "<base64>"}
```

Your plugin responds with one of:
```json
{"op": "intercept_result", "request_id": "req-1", "action": "pass"}
{"op": "intercept_result", "request_id": "req-1", "action": "drop"}
{"op": "intercept_result", "request_id": "req-1", "action": "modify", "payload": "<new base64>"}
```

**Priority:** lower numbers run first. Remote interceptors are clamped to priority ≥ 100. In-process kernel interceptors can use lower values.

**Timeout:** remote interceptors have a max timeout of 2 seconds. If your interceptor doesn't respond in time, the event passes through unchanged.

## Writing a custom delivery channel

A custom delivery channel lets you route events to external systems (MQTT, Kafka, webhooks with custom logic, etc.).

Register via WebSocket:
```json
{"op": "register_channel_type", "channel_type": "mqtt"}
```

The server will then forward matching events as:
```json
{"op": "deliver", "request_id": "req-1", "channel_type": "mqtt", "subject": "iot.sensor.temp", "payload": "<base64>"}
```

Respond with:
```json
{"op": "deliver_result", "request_id": "req-1", "success": true}
```

## Choosing interceptor priorities

| Priority range | Intended use |
|---|---|
| 1-99 | Reserved for in-process kernel interceptors (trusted) |
| 100-199 | Security interceptors (encryption, auth, rate limiting) |
| 200-299 | Enrichment interceptors (scope tagging, metadata injection) |
| 300-399 | Transformation interceptors (format conversion, filtering) |
| 400+ | Observation interceptors (logging, metrics, audit trails) |

Remote interceptors are automatically clamped to priority ≥ 100.

## Testing your plugin

```bash
# Start the kernel (boots the eventbus + WS server)
cargo run -p seidrum-kernel -- serve

# In another terminal, start your plugin
cargo run -p my-plugin

# Or run with a custom bus URL
BUS_URL=ws://127.0.0.1:9000 cargo run -p my-plugin
```

For integration tests, use `seidrum-eventbus` test utilities:

```rust
use seidrum_eventbus::test_utils::test_bus_with_transports;

#[tokio::test]
async fn test_my_interceptor() {
    let env = test_bus_with_transports().await;
    // ... register interceptor, publish events, verify behavior
}
```

## Reference

- **Event subjects**: see `docs/EVENT_CATALOG.md`
- **Plugin registration**: see `docs/PLUGIN_SPEC.md`
- **Eventbus architecture**: see `crates/core/seidrum-eventbus/ARCHITECTURE.md`
- **Interceptor protocol**: see `crates/core/seidrum-eventbus/PLAN.md` Phase 4 + Phase 6
