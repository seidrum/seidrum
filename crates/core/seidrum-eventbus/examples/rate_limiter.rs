//! Example: Rate limiter interceptor (Phase 6 D2)
//!
//! A sync interceptor at priority 110 on `channel.*.inbound` that
//! drops events from users exceeding a per-user rate limit.
//!
//! No subject rewiring needed — the interceptor sits in the dispatch
//! chain and drops events before downstream plugins see them.
//!
//! # Running
//!
//! ```bash
//! cargo run --example rate_limiter
//! ```

use seidrum_eventbus::dispatch::InterceptResult;
use seidrum_eventbus::storage::memory_store::InMemoryEventStore;
use seidrum_eventbus::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Per-user rate limiter state.
struct RateLimiterInterceptor {
    /// Max events per window per user.
    max_per_window: usize,
    /// Window duration.
    window: Duration,
    /// user_id → (window_start, count)
    state: Mutex<HashMap<String, (Instant, usize)>>,
}

impl RateLimiterInterceptor {
    fn new(max_per_window: usize, window: Duration) -> Self {
        Self {
            max_per_window,
            window,
            state: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl Interceptor for RateLimiterInterceptor {
    async fn intercept(&self, _subject: &str, payload: &mut Vec<u8>) -> InterceptResult {
        // Extract user_id from the payload (assumes JSON with a
        // "user_id" field — a real interceptor would parse
        // EventEnvelope → ChannelInbound).
        let user_id = serde_json::from_slice::<serde_json::Value>(payload)
            .ok()
            .and_then(|v| v.get("user_id").and_then(|u| u.as_str()).map(String::from))
            .unwrap_or_else(|| "anonymous".to_string());

        let mut state = self.state.lock().await;
        let now = Instant::now();
        let entry = state.entry(user_id.clone()).or_insert((now, 0));

        // Reset window if expired.
        if now.duration_since(entry.0) > self.window {
            *entry = (now, 0);
        }

        entry.1 += 1;

        if entry.1 > self.max_per_window {
            tracing::warn!(
                user_id = %user_id,
                count = entry.1,
                max = self.max_per_window,
                "rate limit exceeded, dropping event"
            );
            InterceptResult::Drop
        } else {
            InterceptResult::Pass
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await?;

    // Allow 3 events per 10 seconds per user.
    let limiter = Arc::new(RateLimiterInterceptor::new(3, Duration::from_secs(10)));
    bus.intercept("channel.*.inbound", 110, limiter, None)
        .await?;

    // Subscribe to see what gets through.
    let opts = SubscribeOpts {
        priority: 200,
        mode: SubscriptionMode::Async,
        channel: ChannelConfig::InProcess,
        timeout: Duration::from_secs(5),
        filter: None,
    };
    let mut sub = bus.subscribe("channel.*.inbound", opts).await?;

    // Send 5 events from user "alice" — only 3 should get through.
    for i in 1..=5 {
        let payload = serde_json::json!({
            "user_id": "alice",
            "text": format!("message {}", i),
        });
        let bytes = serde_json::to_vec(&payload)?;
        bus.publish("channel.telegram.inbound", &bytes).await?;
        println!("Published message {} from alice", i);
    }

    // Collect what got through (with a short timeout for remaining).
    let mut received = 0;
    loop {
        match tokio::time::timeout(Duration::from_millis(200), sub.rx.recv()).await {
            Ok(Some(_)) => received += 1,
            _ => break,
        }
    }

    println!("Alice sent 5 messages, {} got through (limit: 3)", received);
    assert_eq!(received, 3, "rate limiter should have dropped 2");
    println!("✓ Rate limiter works!");

    Ok(())
}
