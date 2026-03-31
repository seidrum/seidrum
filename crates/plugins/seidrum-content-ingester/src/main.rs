use std::collections::HashMap;

use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use seidrum_common::events::{ChannelInbound, ContentStoreRequest, EventEnvelope, PluginRegister};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(
    name = "seidrum-content-ingester",
    about = "Seidrum content ingester plugin"
)]
struct Cli {
    /// NATS server URL
    #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4222")]
    nats_url: String,

    /// Embedding provider
    #[arg(long, env = "EMBEDDING_PROVIDER", default_value = "openai")]
    embedding_provider: String,

    /// OpenAI API key for embeddings
    #[arg(long, env = "OPENAI_API_KEY", default_value = "")]
    openai_api_key: String,
}

// ---------------------------------------------------------------------------
// OpenAI embedding types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct EmbeddingRequest {
    model: String,
    input: String,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    embedding: Vec<f64>,
}

/// Call OpenAI embeddings API to generate a vector for the given text.
async fn generate_embedding(
    client: &reqwest::Client,
    api_key: &str,
    text: &str,
) -> Result<Vec<f64>> {
    let req = EmbeddingRequest {
        model: "text-embedding-3-small".to_string(),
        input: text.to_string(),
    };

    let resp = client
        .post("https://api.openai.com/v1/embeddings")
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&req)
        .send()
        .await
        .context("failed to call OpenAI embeddings API")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("OpenAI embeddings API returned {status}: {body}");
    }

    let embedding_resp: EmbeddingResponse = resp
        .json()
        .await
        .context("failed to parse OpenAI embedding response")?;

    embedding_resp
        .data
        .into_iter()
        .next()
        .map(|d| d.embedding)
        .context("no embedding returned from OpenAI")
}

/// Extract the channel (platform) name from a NATS subject like
/// `channel.telegram.inbound` -> `"telegram"`.
fn extract_channel_from_subject(subject: &str) -> Option<&str> {
    let parts: Vec<&str> = subject.split('.').collect();
    if parts.len() >= 3 && parts[0] == "channel" && parts[2] == "inbound" {
        Some(parts[1])
    } else {
        None
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    let has_api_key = !cli.openai_api_key.is_empty();
    if !has_api_key {
        warn!("No OPENAI_API_KEY provided; embeddings will be skipped");
    }

    info!(
        nats_url = %cli.nats_url,
        embedding_provider = %cli.embedding_provider,
        has_api_key,
        "Starting seidrum-content-ingester plugin..."
    );

    // Connect to NATS
    let nats = async_nats::connect(&cli.nats_url)
        .await
        .context("failed to connect to NATS")?;
    info!("Connected to NATS");

    // Register with kernel
    let register = PluginRegister {
        id: "content-ingester".to_string(),
        name: "Content Ingester".to_string(),
        version: "0.1.0".to_string(),
        description: "Ingests channel messages and stores them in the brain".to_string(),
        consumes: vec!["channel.*.inbound".to_string()],
        produces: vec!["brain.content.store".to_string()],
        health_subject: "plugin.content-ingester.health".to_string(),
        consumed_event_types: vec![],
        produced_event_types: vec![],
        config_schema: None,
    };

    let register_envelope =
        EventEnvelope::new("plugin.register", "content-ingester", None, None, &register)?;
    let register_bytes = serde_json::to_vec(&register_envelope)?;
    nats.publish("plugin.register", register_bytes.into())
        .await
        .context("failed to publish plugin.register")?;
    info!("Published plugin.register");

    // Subscribe to channel.*.inbound
    let mut sub = nats
        .subscribe("channel.*.inbound")
        .await
        .context("failed to subscribe to channel.*.inbound")?;
    info!("Subscribed to channel.*.inbound");

    let http_client = reqwest::Client::new();

    while let Some(msg) = sub.next().await {
        let subject = msg.subject.to_string();

        let channel = match extract_channel_from_subject(&subject) {
            Some(c) => c.to_string(),
            None => {
                warn!(subject = %subject, "Could not extract channel from subject");
                continue;
            }
        };

        // Parse the event envelope
        let envelope: EventEnvelope = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(err) => {
                error!(%err, subject = %subject, "Failed to parse EventEnvelope");
                continue;
            }
        };

        // Extract ChannelInbound from the payload
        let inbound: ChannelInbound = match serde_json::from_value(envelope.payload.clone()) {
            Ok(i) => i,
            Err(err) => {
                error!(%err, event_id = %envelope.id, "Failed to parse ChannelInbound payload");
                continue;
            }
        };

        info!(
            event_id = %envelope.id,
            channel = %channel,
            user_id = %inbound.user_id,
            text_len = inbound.text.len(),
            "Received channel inbound event"
        );

        // Optionally generate embedding
        if has_api_key {
            match generate_embedding(&http_client, &cli.openai_api_key, &inbound.text).await {
                Ok(embedding) => {
                    info!(
                        event_id = %envelope.id,
                        dims = embedding.len(),
                        "Generated embedding"
                    );
                }
                Err(err) => {
                    warn!(
                        %err,
                        event_id = %envelope.id,
                        "Failed to generate embedding; proceeding without it"
                    );
                }
            }
        }

        // Build metadata
        let mut metadata: HashMap<String, String> = HashMap::new();
        metadata.insert("user_id".to_string(), inbound.user_id.clone());
        metadata.insert("platform".to_string(), inbound.platform.clone());
        if let Some(ref reply_to) = inbound.reply_to {
            metadata.insert("reply_to".to_string(), reply_to.clone());
        }
        // Merge any metadata from the original event
        for (k, v) in &inbound.metadata {
            metadata.insert(k.clone(), v.clone());
        }

        // Build ContentStoreRequest — propagate user_id for multi-tenant isolation
        let store_req = ContentStoreRequest {
            content_type: "message".to_string(),
            channel: channel.clone(),
            channel_id: inbound.chat_id.clone(),
            raw_text: inbound.text.clone(),
            timestamp: envelope.timestamp,
            metadata,
            generate_embedding: true,
            user_id: Some(inbound.user_id.clone()),
        };

        let store_envelope = EventEnvelope::new(
            "brain.content.store",
            "content-ingester",
            envelope.correlation_id.clone(),
            envelope.scope.clone(),
            &store_req,
        )?;
        let store_bytes = serde_json::to_vec(&store_envelope)?;

        if let Err(err) = nats
            .publish("brain.content.store", store_bytes.into())
            .await
        {
            error!(
                %err,
                event_id = %envelope.id,
                "Failed to publish brain.content.store"
            );
            continue;
        }

        info!(
            event_id = %envelope.id,
            channel = %channel,
            "Published brain.content.store"
        );
    }

    Ok(())
}
