pub mod builtin_capabilities;
pub mod guardrails;
pub mod service;

/// NATS request/reply helper shared across consciousness modules.
pub(crate) async fn nats_request<T: serde::Serialize, R: serde::de::DeserializeOwned>(
    nats: &async_nats::Client,
    subject: &str,
    payload: &T,
) -> anyhow::Result<R> {
    let bytes = serde_json::to_vec(payload)?;
    let response = nats.request(subject.to_string(), bytes.into()).await?;
    let result: R = serde_json::from_slice(&response.payload)?;
    Ok(result)
}
