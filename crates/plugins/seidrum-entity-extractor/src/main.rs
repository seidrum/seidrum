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

    /// Google API key (falls back to OpenClaw auth-profiles.json)
    #[arg(long, env = "GOOGLE_API_KEY")]
    google_api_key: Option<String>,

    /// Model to use for entity extraction
    #[arg(long, env = "EXTRACTION_MODEL", default_value = "gemini-2.5-flash")]
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

/// Gemini API request.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    generation_config: GeminiGenerationConfig,
}

#[derive(Serialize, Debug)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

#[derive(Serialize, Debug)]
struct GeminiPart {
    text: String,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerationConfig {
    max_output_tokens: u32,
}

/// Gemini API response.
#[derive(Deserialize, Debug)]
struct GeminiResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
}

#[derive(Deserialize, Debug)]
struct GeminiCandidate {
    content: GeminiCandidateContent,
}

#[derive(Deserialize, Debug)]
struct GeminiCandidateContent {
    parts: Vec<GeminiResponsePart>,
}

#[derive(Deserialize, Debug)]
struct GeminiResponsePart {
    text: Option<String>,
}

/// Resolve Google API key from env var or OpenClaw auth-profiles.json.
fn resolve_google_api_key(cli_key: &Option<String>) -> Result<String> {
    if let Some(key) = cli_key {
        if !key.is_empty() {
            return Ok(key.clone());
        }
    }
    let auth_path = dirs::home_dir()
        .map(|h| h.join(".openclaw/agents/main/agent/auth-profiles.json"))
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    let content = std::fs::read_to_string(&auth_path)
        .map_err(|e| anyhow::anyhow!("Failed to read OpenClaw auth-profiles.json: {}", e))?;
    let profiles: serde_json::Value = serde_json::from_str(&content)?;
    let key = profiles
        .get("profiles").and_then(|p| p.get("google:default"))
        .and_then(|v| v.get("key"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("No google:default.key in OpenClaw auth-profiles.json"))?;
    Ok(key.to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    let api_key = resolve_google_api_key(&args.google_api_key)?;

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
            &api_key,
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

    let request_body = GeminiRequest {
        contents: vec![GeminiContent {
            role: "user".to_string(),
            parts: vec![GeminiPart { text: prompt }],
        }],
        generation_config: GeminiGenerationConfig {
            max_output_tokens: 4096,
        },
    };

    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
        model, api_key
    );

    let response = http
        .post(&url)
        .header("content-type", "application/json")
        .timeout(Duration::from_secs(60))
        .json(&request_body)
        .send()
        .await
        .context("Gemini API request failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Gemini API returned {}: {}", status, body);
    }

    let api_response: GeminiResponse = response
        .json()
        .await
        .context("Failed to parse Gemini API response")?;

    let text_content = api_response
        .candidates
        .first()
        .and_then(|c| c.content.parts.first())
        .and_then(|p| p.text.as_ref())
        .context("No text content in Gemini response")?;

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
