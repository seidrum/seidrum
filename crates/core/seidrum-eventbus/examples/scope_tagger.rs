//! Example: Scope tagger interceptor (Phase 6 D3)
//!
//! A sync interceptor at priority 200 on `channel.*.inbound` that
//! annotates events with user scope context. Downstream plugins
//! receive enriched events automatically — no code changes needed.
//!
//! This demonstrates the interceptor's ability to **modify** the
//! payload in-flight, adding metadata that downstream consumers
//! can rely on without having to fetch it themselves.
//!
//! # Running
//!
//! ```bash
//! cargo run --example scope_tagger
//! ```

use seidrum_eventbus::dispatch::InterceptResult;
use seidrum_eventbus::storage::memory_store::InMemoryEventStore;
use seidrum_eventbus::*;
use std::sync::Arc;
use std::time::Duration;

/// Scope tagger: injects `"scope": "personal"` into every inbound
/// event's JSON payload. A real implementation would look up the
/// user's scope from the brain service via request/reply.
struct ScopeTaggerInterceptor;

#[async_trait::async_trait]
impl Interceptor for ScopeTaggerInterceptor {
    async fn intercept(&self, _subject: &str, payload: &mut Vec<u8>) -> InterceptResult {
        // Try to parse the payload as JSON and inject the scope field.
        if let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(payload) {
            if let Some(obj) = value.as_object_mut() {
                obj.insert(
                    "scope".to_string(),
                    serde_json::Value::String("personal".to_string()),
                );
                if let Ok(new_bytes) = serde_json::to_vec(&value) {
                    *payload = new_bytes;
                    return InterceptResult::Modified;
                }
            }
        }
        // Non-JSON payloads pass through unchanged.
        InterceptResult::Pass
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await?;

    // Register the scope tagger at priority 200 (after rate limiter at 110).
    bus.intercept(
        "channel.*.inbound",
        200,
        Arc::new(ScopeTaggerInterceptor),
        None,
    )
    .await?;

    // Subscribe — the downstream consumer sees the enriched payload.
    let opts = SubscribeOpts {
        priority: 300,
        mode: SubscriptionMode::Async,
        channel: ChannelConfig::InProcess,
        timeout: Duration::from_secs(5),
        filter: None,
    };
    let mut sub = bus.subscribe("channel.*.inbound", opts).await?;

    // Publish an event WITHOUT a scope field.
    let payload = serde_json::json!({
        "user_id": "alice",
        "text": "hello world",
    });
    let bytes = serde_json::to_vec(&payload)?;
    println!("Publishing: {}", serde_json::to_string_pretty(&payload)?);

    bus.publish("channel.telegram.inbound", &bytes).await?;

    // The subscriber receives the event WITH the scope field injected.
    if let Some(event) = sub.rx.recv().await {
        let enriched: serde_json::Value = serde_json::from_slice(&event.payload)?;
        println!("Received:   {}", serde_json::to_string_pretty(&enriched)?);
        assert_eq!(enriched["scope"], "personal");
        assert_eq!(enriched["user_id"], "alice");
        assert_eq!(enriched["text"], "hello world");
        println!("✓ Scope tagger works — downstream sees enriched payload!");
    }

    Ok(())
}
