use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use seidrum_common::events::{
    BrainQueryRequest, EntityUpserted, EventEnvelope, FactUpsertRequest, PluginRegister,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{error, info, warn};

const PLUGIN_ID: &str = "fact-extractor";
const PLUGIN_NAME: &str = "Fact Extractor";
const PLUGIN_VERSION: &str = "0.1.0";

#[derive(Parser, Debug)]
#[command(name = "seidrum-fact-extractor")]
#[command(about = "Extracts factual claims from content about entities using LLM")]
struct Args {
    /// NATS server URL
    #[arg(long, env = "NATS_URL", default_value = "nats://127.0.0.1:4222")]
    nats_url: String,

    /// Google API key (falls back to OpenClaw auth-profiles.json)
    #[arg(long, env = "GOOGLE_API_KEY")]
    google_api_key: Option<String>,

    /// Model to use for fact extraction
    #[arg(long, env = "EXTRACTION_MODEL", default_value = "gemini-2.5-flash")]
    extraction_model: String,
}

/// A fact extracted by the LLM.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct ExtractedFact {
    /// Entity key that the fact is about
    subject: String,
    /// Relationship or property predicate (e.g. "works_at", "located_in", "uses_tool")
    predicate: String,
    /// Another entity key, if the fact relates two entities
    object: Option<String>,
    /// Literal value, if the fact assigns a value
    value: Option<String>,
    /// Extraction confidence from 0.0 to 1.0
    confidence: f64,
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
        .get("profiles")
        .and_then(|p| p.get("google:default"))
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
        "Starting fact extractor plugin"
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
        description: "Extracts factual claims from content about entities using LLM".to_string(),
        consumes: vec!["brain.entity.upserted".to_string()],
        produces: vec!["brain.fact.upsert".to_string()],
        health_subject: format!("plugin.{PLUGIN_ID}.health"),
        consumed_event_types: vec![],
        produced_event_types: vec![],
        config_schema: None,
    };

    let register_envelope =
        EventEnvelope::new("plugin.register", PLUGIN_ID, None, None, &register)?;

    client
        .publish(
            "plugin.register",
            serde_json::to_vec(&register_envelope)?.into(),
        )
        .await
        .context("Failed to publish plugin.register")?;

    info!("Plugin registered");

    // Subscribe to brain.entity.upserted
    let mut subscriber = client
        .subscribe("brain.entity.upserted")
        .await
        .context("Failed to subscribe to brain.entity.upserted")?;

    info!("Subscribed to brain.entity.upserted, waiting for events...");

    let http_client = reqwest::Client::new();

    while let Some(msg) = subscriber.next().await {
        let envelope: EventEnvelope = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(err) => {
                warn!(%err, "Failed to deserialize event envelope, skipping");
                continue;
            }
        };

        let entity_upserted: EntityUpserted = match serde_json::from_value(envelope.payload.clone())
        {
            Ok(eu) => eu,
            Err(err) => {
                warn!(%err, "Failed to deserialize EntityUpserted payload, skipping");
                continue;
            }
        };

        info!(
            entity_key = %entity_upserted.entity_key,
            entity_name = %entity_upserted.name,
            entity_type = %entity_upserted.entity_type,
            "Processing entity upserted event"
        );

        // Only process if we have a source content reference
        let source_content = match &entity_upserted.source_content {
            Some(sc) => sc.clone(),
            None => {
                info!(
                    entity_key = %entity_upserted.entity_key,
                    "No source_content on entity, skipping fact extraction"
                );
                continue;
            }
        };

        if let Err(err) = process_entity(
            &client,
            &http_client,
            &api_key,
            &args.extraction_model,
            &entity_upserted,
            &source_content,
            &envelope.correlation_id,
            &envelope.scope,
        )
        .await
        {
            error!(
                entity_key = %entity_upserted.entity_key,
                %err,
                "Failed to extract facts"
            );
        }
    }

    Ok(())
}

async fn process_entity(
    nats: &async_nats::Client,
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    entity: &EntityUpserted,
    source_content_key: &str,
    correlation_id: &Option<String>,
    scope: &Option<String>,
) -> Result<()> {
    // Step 1: Fetch the content text from the kernel via brain.query.request
    let query_req = BrainQueryRequest {
        query_type: "aql".to_string(),
        aql: Some("FOR doc IN content FILTER doc._key == @key RETURN doc.raw_text".to_string()),
        bind_vars: Some(HashMap::from([(
            "key".to_string(),
            serde_json::Value::String(source_content_key.to_string()),
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
                content_key = %source_content_key,
                "No text found for content key"
            );
            return Ok(());
        }
    };

    if raw_text.is_empty() {
        info!(content_key = %source_content_key, "Empty content text, skipping");
        return Ok(());
    }

    // Step 2: Call LLM for fact extraction
    let facts = extract_facts_via_llm(http, api_key, model, &raw_text, entity).await?;

    info!(
        entity_key = %entity.entity_key,
        count = facts.len(),
        "Extracted facts"
    );

    // Step 3: Publish brain.fact.upsert for each fact
    for fact in &facts {
        let upsert = FactUpsertRequest {
            subject: fact.subject.clone(),
            predicate: fact.predicate.clone(),
            object: fact.object.clone(),
            value: fact.value.clone(),
            confidence: fact.confidence,
            source_content: source_content_key.to_string(),
            valid_from: None,
        };

        let upsert_envelope = EventEnvelope::new(
            "brain.fact.upsert",
            PLUGIN_ID,
            correlation_id.clone(),
            scope.clone(),
            &upsert,
        )?;

        nats.publish(
            "brain.fact.upsert",
            serde_json::to_vec(&upsert_envelope)?.into(),
        )
        .await
        .context("Failed to publish brain.fact.upsert")?;

        info!(
            subject = %fact.subject,
            predicate = %fact.predicate,
            confidence = %fact.confidence,
            "Published fact upsert"
        );
    }

    Ok(())
}

async fn extract_facts_via_llm(
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    text: &str,
    entity: &EntityUpserted,
) -> Result<Vec<ExtractedFact>> {
    let prompt = format!(
        r#"Extract factual claims from the following text about the entity "{entity_name}" (key: "{entity_key}", type: {entity_type}).

Return a JSON array of facts. Each fact has:
- "subject": entity key that the fact is about (use "{entity_key}" for the primary entity, or another entity key if relevant)
- "predicate": the relationship or property (e.g. "works_at", "located_in", "uses_tool", "has_role", "knows", "owns", "interested_in", "depends_on", "status_is", "prefers")
- "object": another entity key if the fact relates two entities, or null if the fact assigns a literal value
- "value": a literal string value if the fact is a property, or null if it relates two entities
- "confidence": your confidence from 0.0 to 1.0 that this fact is true based on the text

Rules:
- Every fact must have either "object" or "value" set (not both, not neither)
- Use snake_case for predicates
- Use descriptive entity keys (e.g. "entity_acme_corp", "entity_jane_doe")
- Only extract facts clearly supported by the text
- Return ONLY a valid JSON array, no other text. If no facts found, return []

Example output:
[
  {{"subject": "entity_jane", "predicate": "works_at", "object": "entity_acme_corp", "value": null, "confidence": 0.95}},
  {{"subject": "entity_jane", "predicate": "has_role", "object": null, "value": "Senior Engineer", "confidence": 0.9}},
  {{"subject": "entity_jane", "predicate": "located_in", "object": "entity_san_francisco", "value": null, "confidence": 0.8}}
]

Text to analyze:
{text}"#,
        entity_name = entity.name,
        entity_key = entity.entity_key,
        entity_type = entity.entity_type,
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

    // Parse the JSON from the LLM response -- strip any markdown fencing if present
    let json_str = text_content
        .trim()
        .strip_prefix("```json")
        .or_else(|| text_content.trim().strip_prefix("```"))
        .unwrap_or(text_content.trim());
    let json_str = json_str.strip_suffix("```").unwrap_or(json_str).trim();

    let facts: Vec<ExtractedFact> =
        serde_json::from_str(json_str).context("Failed to parse LLM fact extraction output")?;

    // Validate facts: each must have either object or value
    let valid_facts: Vec<ExtractedFact> = facts
        .into_iter()
        .filter(|f| {
            let has_object = f.object.is_some();
            let has_value = f.value.is_some();
            if !has_object && !has_value {
                warn!(
                    subject = %f.subject,
                    predicate = %f.predicate,
                    "Dropping fact with neither object nor value"
                );
                return false;
            }
            if f.confidence < 0.0 || f.confidence > 1.0 {
                warn!(
                    subject = %f.subject,
                    predicate = %f.predicate,
                    confidence = %f.confidence,
                    "Dropping fact with invalid confidence"
                );
                return false;
            }
            true
        })
        .collect();

    Ok(valid_facts)
}
