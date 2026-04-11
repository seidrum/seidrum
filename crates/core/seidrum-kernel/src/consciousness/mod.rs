pub mod builtin_capabilities;
pub mod guardrails;
pub mod service;

/// NATS request/reply helper shared across consciousness modules.
pub(crate) async fn nats_request<T: serde::Serialize, R: serde::de::DeserializeOwned>(
    nats: &seidrum_common::bus_client::BusClient,
    subject: &str,
    payload: &T,
) -> anyhow::Result<R> {
    let bytes = serde_json::to_vec(payload)?;
    let response = nats.request_bytes(subject.to_string(), bytes).await?;
    let result: R = serde_json::from_slice(&response)?;
    Ok(result)
}
