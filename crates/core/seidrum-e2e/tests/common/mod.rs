//! Shared test helpers for E2E tests.

use std::time::Duration;

/// Connect to NATS using env vars or defaults.
pub async fn connect_nats() -> async_nats::Client {
    let url = std::env::var("TEST_NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".into());
    async_nats::connect(&url)
        .await
        .expect("Failed to connect to NATS. Is it running?")
}

/// NATS request/reply helper with timeout.
pub async fn nats_request<T: serde::Serialize, R: serde::de::DeserializeOwned>(
    nats: &async_nats::Client,
    subject: &str,
    payload: &T,
) -> R {
    let bytes = serde_json::to_vec(payload).unwrap();
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        nats.request(subject.to_string(), bytes.into()),
    )
    .await
    .expect("NATS request timed out (5s)")
    .expect("NATS request failed");
    serde_json::from_slice(&response.payload).expect("Failed to parse NATS response")
}

/// Generate a unique test ID to avoid collisions between test runs.
#[allow(dead_code)]
pub fn test_id(prefix: &str) -> String {
    format!("{}-{}", prefix, ulid::Ulid::new().to_string())
}
