//! Shared test helpers for E2E tests.

use std::time::Duration;

/// Connect to the bus using env vars or defaults.
pub async fn connect_bus() -> seidrum_common::bus_client::BusClient {
    let url = std::env::var("TEST_BUS_URL").unwrap_or_else(|_| "ws://localhost:9000".into());
    seidrum_common::bus_client::BusClient::connect(&url, "e2e-test")
        .await
        .expect("Failed to connect to bus. Is it running?")
}

/// Bus request/reply helper with timeout.
pub async fn bus_request<T: serde::Serialize, R: serde::de::DeserializeOwned>(
    nats: &seidrum_common::bus_client::BusClient,
    subject: &str,
    payload: &T,
) -> R {
    tokio::time::timeout(Duration::from_secs(5), nats.request(subject, payload))
        .await
        .expect("bus request timed out (5s)")
        .expect("bus request failed")
}

/// Generate a unique test ID to avoid collisions between test runs.
#[allow(dead_code)]
pub fn test_id(prefix: &str) -> String {
    format!("{}-{}", prefix, ulid::Ulid::new().to_string())
}
