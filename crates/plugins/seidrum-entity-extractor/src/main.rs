use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use seidrum_common::events::{
    BrainQueryRequest, ContentStored, EntityUpsertRequest, EventEnvelope, PluginRegister,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{error, info, warn};

const PLUGIN_ID: &str = "entity-extractor";
const PLUGIN_NAME: &str = "Entity Extractor";
const PLUGIN_VERSION: &str = "0.1.0";

#[derive(Parser, Debug)]
#[command(name = "seidrum-entity-extractor")]
#[command(about = "Extracts entities from stored content using LLM")]
struct Args {
    /// NATS server URL
    #[arg(long, env = "NATS_URL", default_value = "nats://127.0.0.1:4222")]
    nats_url: String,

    /// Anthropic API key
    #[arg(long, env = "ANTHROPIC_API_KEY")]
    anthropic_api_key: String,

    /// Model to use for entity extraction
    #[arg(long, env = "EXTRACTION_MODEL", default_value = "claude-haiku-4-5-20251001")]
    extraction_model: String,
}

/// An entity extracted by the LLM.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct ExtractedEntity {
    name: String,
    #[serde(rename = "type")]
    entity_type: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    properties: HashMap<String, String>,
}

/// Anthropic Messages API request.
#[derive(Serialize, Debug)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<AnthropicMessage>,
}

#[derive(Serialize, Debug)]
struct AnthropicMessage {
    role: String,
    content: String,
}

/// Anthropic Messages API response.
#[derive(Deserialize, Debug)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
}

#[derive(Deserialize, Debug)]
struct AnthropicContent {
    text: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    info!(
        plugin = PLUGIN_ID,
        model = %args.extraction_model,
        "Starting entity extractor plugin"
    );

    // Connect to NATS
    let client = async_nats::connect(&args.nats_url)
        .await
        .context("Failed to connect to NATS")?;

    info!(url = %args.nats_url, "Connected to NATS");

    // Register plugin
    let register = PluginRegister {
        id: PLUGIN_ID.to_string(),
        name: PLUGIN_NAME.to_string(),
        version: PLUGIN_VERSION.to_string(),
        description: "Extracts entities from stored content using LLM".to_string(),
        consumes: vec!["brain.content.stored".to_string()],
        produces: vec!["brain.entity.upsert".to_string()],
        health_subject: format!("plugin.{}.health", PLUGIN_ID),
    };

    let register_envelope = EventEnvelope::new(
        "plugin.register",
        PLUGIN_ID,
        None,
        None,
        &register,
    )?;

    client
        .publish(
            "plugin.register",
            serde_json::to_vec(&register_envelope)?.into(),
        )
        .await
        .context("Failed to publish plugin.register")?;

    info!("Plugin registered");

    // Subscribe to brain.content.stored
    let mut subscriber = client
        .subscribe("brain.content.stored")
        .await
        .context("Failed to subscribe to brain.content.stored")?;

    info!("Subscribed to brain.content.stored, waiting for events...");

    let http_client = reqwest::Client::new();

    while let Some(msg) = subscriber.next().await {
        let envelope: EventEnvelope = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(err) => {
                warn!(%err, "Failed to deserialize event envelope, skipping");
                continue;
            }
        };

        let content_stored: ContentStored = match serde_json::from_value(envelope.payload.clone())
        {
            Ok(cs) => cs,
            Err(err) => {
                warn!(%err, "Failed to deserialize ContentStored payload, skipping");
                continue;
            }
        };

        info!(
            content_key = %content_stored.content_key,
            content_type = %content_stored.content_type,
            "Processing content stored event"
        );

        if let Err(err) = process_content(
            &client,
            &http_client,
            &args.anthropic_api_key,
            &args.extraction_model,
            &content_stored,
            &envelope.correlation_id,
            &envelope.scope,
        )
        .await
        {
            error!(
                content_key = %content_stored.content_key,
                %err,
                "Failed to extract entities"
            );
        }
    }

    Ok(())
}

async fn process_content(
    nats: &async_nats::Client,
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    content_stored: &ContentStored,
    correlation_id: &Option<String>,
    scope: &Option<String>,
) -> Result<()> {
    // Step 1: Fetch the content text from the kernel via brain.query.request
    let query_req = BrainQueryRequest {
        query_type: "aql".to_string(),
        aql: Some(
            "FOR doc IN content FILTER doc._key == @key RETURN doc.raw_text".to_string(),
        ),
        bind_vars: Some(HashMap::from([(
            "key".to_string(),
            serde_json::Value::String(content_stored.content_key.clone()),
        )])),
        embedding: None,
        collection: None,
        limit: None,
        start_vertex: None,
        direction: None,
        depth: None,
        query_text: None,
        max_facts: None,
        graph_depth: None,
        min_confidence: None,
    };

    let query_envelope = EventEnvelope::new(
        "brain.query.request",
        PLUGIN_ID,
        correlation_id.clone(),
        scope.clone(),
        &query_req,
    )?;

    let response = nats
        .request(
            "brain.query.request",
            serde_json::to_vec(&query_envelope)?.into(),
        )
        .await
        .context("brain.query.request timed out or failed")?;

    let response_envelope: EventEnvelope = serde_json::from_slice(&response.payload)
        .context("Failed to deserialize brain.query.response envelope")?;

    let query_response: seidrum_common::events::BrainQueryResponse =
        serde_json::from_value(response_envelope.payload)
            .context("Failed to deserialize BrainQueryResponse")?;

    // Extract the raw_text from the results
    let raw_text = match &query_response.results {
        serde_json::Value::Array(arr) if !arr.is_empty() => {
            arr[0].as_str().unwrap_or("").to_string()
        }
        _ => {
            warn!(
                content_key = %content_stored.content_key,
                "No text found for content key"
            );
            return Ok(());
        }
    };

    if raw_text.is_empty() {
        info!(content_key = %content_stored.content_key, "Empty content text, skipping");
        return Ok(());
    }

    // Step 2: Call LLM for entity extraction
    let entities = extract_entities_via_llm(http, api_key, model, &raw_text).await?;

    info!(
        content_key = %content_stored.content_key,
        count = entities.len(),
        "Extracted entities"
    );

    // Step 3: Publish brain.entity.upsert for each entity
    for entity in &entities {
        let upsert = EntityUpsertRequest {
            entity_key: None, // Let kernel handle create/lookup
            entity_type: entity.entity_type.clone(),
            name: entity.name.clone(),
            aliases: entity.aliases.clone(),
            properties: entity.properties.clone(),
            source_content: Some(content_stored.content_key.clone()),
            mentions_content: Some(content_stored.content_key.clone()),
            mention_type: Some("extracted".to_string()),
        };

        let upsert_envelope = EventEnvelope::new(
            "brain.entity.upsert",
            PLUGIN_ID,
            correlation_id.clone(),
            scope.clone(),
            &upsert,
        )?;

        nats.publish(
            "brain.entity.upsert",
            serde_json::to_vec(&upsert_envelope)?.into(),
        )
        .await
        .context("Failed to publish brain.entity.upsert")?;

        info!(
            name = %entity.name,
            entity_type = %entity.entity_type,
            "Published entity upsert"
        );
    }

    Ok(())
}

async fn extract_entities_via_llm(
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    text: &str,
) -> Result<Vec<ExtractedEntity>> {
    let prompt = format!(
        r#"Extract entities (people, organizations, projects, tools, locations, concepts) from this text. Return JSON array with: name, type, aliases, properties.

Valid entity types: person, organization, project, product, concept, location, tool, channel

Return ONLY a valid JSON array, no other text. If no entities found, return [].

Example output:
[
  {{"name": "Acme Corp", "type": "organization", "aliases": ["Acme"], "properties": {{"industry": "tech"}}}},
  {{"name": "Jane Doe", "type": "person", "aliases": [], "properties": {{"role": "CEO"}}}}
]

Text to analyze:
{text}"#
    );

    let request_body = AnthropicRequest {
        model: model.to_string(),
        max_tokens: 4096,
        messages: vec![AnthropicMessage {
            role: "user".to_string(),
            content: prompt,
        }],
    };

    let response = http
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .timeout(Duration::from_secs(60))
        .json(&request_body)
        .send()
        .await
        .context("Anthropic API request failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Anthropic API returned {}: {}", status, body);
    }

    let api_response: AnthropicResponse = response
        .json()
        .await
        .context("Failed to parse Anthropic API response")?;

    let text_content = api_response
        .content
        .iter()
        .find_map(|c| c.text.as_ref())
        .context("No text content in Anthropic response")?;

    // Parse the JSON from the LLM response — strip any markdown fencing if present
    let json_str = text_content
        .trim()
        .strip_prefix("```json")
        .or_else(|| text_content.trim().strip_prefix("```"))
        .unwrap_or(text_content.trim());
    let json_str = json_str
        .strip_suffix("```")
        .unwrap_or(json_str)
        .trim();

    let entities: Vec<ExtractedEntity> =
        serde_json::from_str(json_str).context("Failed to parse LLM entity extraction output")?;

    Ok(entities)
}
