//! Example: Encryption interceptor (Phase 6 D1)
//!
//! A sync interceptor at priority 100 on `channel.*.inbound` that
//! "decrypts" inbound message payloads before downstream plugins see
//! them. A corresponding interceptor on `channel.*.outbound` "encrypts"
//! outbound payloads before they leave the bus.
//!
//! This example uses a trivial XOR cipher for demonstration. A real
//! implementation would use `aes-gcm` or `chacha20poly1305`.
//!
//! # Running
//!
//! ```bash
//! cargo run --example encryption_interceptor
//! ```

use seidrum_eventbus::dispatch::InterceptResult;
use seidrum_eventbus::storage::memory_store::InMemoryEventStore;
use seidrum_eventbus::*;
use std::sync::Arc;
use std::time::Duration;

/// Trivial XOR "cipher" for demonstration. NOT cryptographically secure.
fn xor_cipher(data: &[u8], key: u8) -> Vec<u8> {
    data.iter().map(|b| b ^ key).collect()
}

/// Decrypt interceptor: runs on inbound events, XOR-decrypts the payload.
struct DecryptInterceptor {
    key: u8,
}

#[async_trait::async_trait]
impl Interceptor for DecryptInterceptor {
    async fn intercept(&self, _subject: &str, payload: &mut Vec<u8>) -> InterceptResult {
        *payload = xor_cipher(payload, self.key);
        InterceptResult::Modified
    }
}

/// Encrypt interceptor: runs on outbound events, XOR-encrypts the payload.
struct EncryptInterceptor {
    key: u8,
}

#[async_trait::async_trait]
impl Interceptor for EncryptInterceptor {
    async fn intercept(&self, _subject: &str, payload: &mut Vec<u8>) -> InterceptResult {
        *payload = xor_cipher(payload, self.key);
        InterceptResult::Modified
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new().storage(store).build().await?;

    let key = 0x42; // Shared key

    // Register decrypt interceptor on inbound (priority 100 — runs first)
    bus.intercept(
        "channel.*.inbound",
        100,
        Arc::new(DecryptInterceptor { key }),
        None,
    )
    .await?;

    // Register encrypt interceptor on outbound (priority 100)
    bus.intercept(
        "channel.*.outbound",
        100,
        Arc::new(EncryptInterceptor { key }),
        None,
    )
    .await?;

    // Subscribe to inbound (sees decrypted payload)
    let opts = SubscribeOpts {
        priority: 200,
        mode: SubscriptionMode::Async,
        channel: ChannelConfig::InProcess,
        timeout: Duration::from_secs(5),
        filter: None,
    };
    let mut sub = bus.subscribe("channel.*.inbound", opts).await?;

    // Simulate: publish an "encrypted" inbound message
    let plaintext = b"Hello from Telegram!";
    let encrypted = xor_cipher(plaintext, key);
    println!("Publishing encrypted payload: {:?}", encrypted);

    bus.publish("channel.telegram.inbound", &encrypted).await?;

    // The subscriber should receive the decrypted plaintext
    if let Some(event) = sub.rx.recv().await {
        let decrypted = String::from_utf8_lossy(&event.payload);
        println!("Subscriber received (decrypted): {}", decrypted);
        assert_eq!(event.payload, plaintext);
        println!("✓ Encryption interceptor works!");
    }

    Ok(())
}
