//! Example: Custom delivery channel — MQTT proxy (Phase 6 D4)
//!
//! A plugin registers an `"mqtt"` custom delivery channel type. Other
//! plugins can then request MQTT delivery for IoT events by subscribing
//! with `ChannelConfig::Custom { channel_type: "mqtt", .. }`.
//!
//! This example uses a mock MQTT publisher (prints to stdout). A real
//! implementation would use an MQTT client library like `rumqttc`.
//!
//! # Running
//!
//! ```bash
//! cargo run --example custom_channel
//! ```

use async_trait::async_trait;
use seidrum_eventbus::delivery::{ChannelConfig, DeliveryChannel, DeliveryReceipt, DeliveryResult};
use seidrum_eventbus::storage::memory_store::InMemoryEventStore;
use seidrum_eventbus::*;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Mock MQTT delivery channel that prints events to stdout.
/// A real implementation would connect to an MQTT broker and publish
/// to the configured MQTT topic.
struct MqttChannel;

#[async_trait]
impl DeliveryChannel for MqttChannel {
    async fn deliver(
        &self,
        event: &[u8],
        subject: &str,
        config: &ChannelConfig,
    ) -> DeliveryResult<DeliveryReceipt> {
        let topic = match config {
            ChannelConfig::Custom { config, .. } => config
                .get("mqtt_topic")
                .and_then(|v| v.as_str())
                .unwrap_or("default/topic"),
            _ => "default/topic",
        };

        println!(
            "[MQTT] Publishing to topic '{}': subject={}, payload={} bytes",
            topic,
            subject,
            event.len()
        );

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        Ok(DeliveryReceipt {
            delivered_at: now_ms,
            latency_us: 100,
        })
    }

    async fn cleanup(&self, _config: &ChannelConfig) -> DeliveryResult<()> {
        println!("[MQTT] Channel cleaned up");
        Ok(())
    }

    async fn is_healthy(&self, _config: &ChannelConfig) -> bool {
        true
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let store = Arc::new(InMemoryEventStore::new());
    let bus = EventBusBuilder::new()
        .storage(store)
        .register_channel("mqtt", Arc::new(MqttChannel) as Arc<dyn DeliveryChannel>)
        .build()
        .await?;

    println!("Registered 'mqtt' custom delivery channel");

    // Publish some IoT events — in a real system, a subscriber with
    // ChannelConfig::Custom { channel_type: "mqtt" } would trigger
    // delivery through the MqttChannel. Here we just demonstrate the
    // channel registration and how it would integrate.
    bus.publish(
        "iot.sensor.temperature",
        b"{\"value\": 23.5, \"unit\": \"celsius\"}",
    )
    .await?;
    bus.publish(
        "iot.sensor.humidity",
        b"{\"value\": 65, \"unit\": \"percent\"}",
    )
    .await?;

    println!("Published 2 IoT events");
    println!("✓ Custom delivery channel registered and ready!");
    println!();
    println!("In a real system, plugins would subscribe with:");
    println!("  ChannelConfig::Custom {{ channel_type: \"mqtt\", config: {{\"mqtt_topic\": \"sensors/temp\"}} }}");
    println!("and the eventbus would route matching events through the MqttChannel.");

    Ok(())
}
