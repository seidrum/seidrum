use anyhow::{Context, Result};
use clap::Parser;
use seidrum_common::events::{
    BrainQueryRequest, ContentStored, EventEnvelope, PluginRegister, ScopeAssignRequest,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{error, info, warn};

const PLUGIN_ID: &str = "scope-classifier";
const PLUGIN_NAME: &str = "Scope Classifier";
const PLUGIN_VERSION: &str = "0.1.0";

#[derive(Parser, Debug)]
#[command(name = "seidrum-scope-classifier")]
#[command(about = "Classifies content into scopes using LLM")]
struct Args {
    /// Bus server URL
    #[arg(long, env = "BUS_URL", default_value = "ws://127.0.0.1:9000")]
    bus_url: String,

    /// Google API key (falls back to OpenClaw auth-profiles.json)
    #[arg(long, env = "GOOGLE_API_KEY")]
    google_api_key: Option<String>,

    /// Model to use for scope classification
    #[arg(long, env = "CLASSIFICATION_MODEL", default_value = "gemini-2.5-flash")]
    classification_model: String,
}

/// A scope classification result from the LLM.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct ScopeClassification {
    scope_key: String,
    relevance: f64,
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
        model = %args.classification_model,
        "Starting scope classifier plugin"
    );

    // Connect to NATS
    let client = seidrum_common::bus_client::BusClient::connect(&args.bus_url, "scope-classifier")
        .await
        .context("Failed to connect to NATS")?;

    info!(url = %args.bus_url, "Connected to NATS");

    // Register plugin
    let register = PluginRegister {
        id: PLUGIN_ID.to_string(),
        name: PLUGIN_NAME.to_string(),
        version: PLUGIN_VERSION.to_string(),
        description: "Classifies content into scopes using LLM".to_string(),
        consumes: vec!["brain.content.stored".to_string()],
        produces: vec!["brain.scope.assign".to_string()],
        health_subject: format!("plugin.{}.health", PLUGIN_ID),
        consumed_event_types: vec![],
        produced_event_types: vec![],
        config_schema: None,
    };

    let register_envelope =
        EventEnvelope::new("plugin.register", PLUGIN_ID, None, None, &register)?;

    client
        .publish_bytes("plugin.register", serde_json::to_vec(&register_envelope)?)
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

        let content_stored: ContentStored = match serde_json::from_value(envelope.payload.clone()) {
            Ok(cs) => cs,
            Err(err) => {
                warn!(%err, "Failed to deserialize ContentStored payload, skipping");
                continue;
            }
        };

        info!(
            content_key = %content_stored.content_key,
            content_type = %content_stored.content_type,
            "Processing content stored event for scope classification"
        );

        if let Err(err) = process_content(
            &client,
            &http_client,
            &api_key,
            &args.classification_model,
            &content_stored,
            &envelope.correlation_id,
            &envelope.scope,
        )
        .await
        {
            error!(
                content_key = %content_stored.content_key,
                %err,
                "Failed to classify content into scopes"
            );
        }
    }

    Ok(())
}

async fn process_content(
    nats: &seidrum_common::bus_client::BusClient,
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
        aql: Some("FOR doc IN content FILTER doc._key == @key RETURN doc.raw_text".to_string()),
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
        user_id: None,
    };

    let query_envelope = EventEnvelope::new(
        "brain.query.request",
        PLUGIN_ID,
        correlation_id.clone(),
        scope.clone(),
        &query_req,
    )?;

    let response = nats
        .request_bytes("brain.query.request", serde_json::to_vec(&query_envelope)?)
        .await
        .context("brain.query.request timed out or failed")?;

    let response_envelope: EventEnvelope = serde_json::from_slice(&response)
        .context("Failed to deserialize brain.query.response envelope")?;

    let query_response: seidrum_common::events::BrainQueryResponse =
        serde_json::from_value(response_envelope.payload)
            .context("Failed to deserialize BrainQueryResponse")?;

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

    // Step 2: Fetch active scopes from the brain
    let scopes_req = BrainQueryRequest {
        query_type: "aql".to_string(),
        aql: Some(
            "FOR s IN scopes FILTER s.active == true RETURN { key: s._key, name: s.name, description: s.description }".to_string(),
        ),
        bind_vars: None,
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
        user_id: None,
    };

    let scopes_envelope = EventEnvelope::new(
        "brain.query.request",
        PLUGIN_ID,
        correlation_id.clone(),
        scope.clone(),
        &scopes_req,
    )?;

    let scopes_response = nats
        .request_bytes("brain.query.request", serde_json::to_vec(&scopes_envelope)?)
        .await
        .context("brain.query.request for scopes timed out or failed")?;

    let scopes_response_envelope: EventEnvelope = serde_json::from_slice(&scopes_response)
        .context("Failed to deserialize scopes query response envelope")?;

    let scopes_query_response: seidrum_common::events::BrainQueryResponse =
        serde_json::from_value(scopes_response_envelope.payload)
            .context("Failed to deserialize scopes BrainQueryResponse")?;

    let scopes_json = serde_json::to_string_pretty(&scopes_query_response.results)
        .unwrap_or_else(|_| "[]".to_string());

    if scopes_query_response.count == 0 {
        warn!(
            content_key = %content_stored.content_key,
            "No active scopes found in brain, skipping classification"
        );
        return Ok(());
    }

    // Step 3: Call LLM to classify content into scopes
    let classifications = classify_via_llm(http, api_key, model, &raw_text, &scopes_json).await?;

    info!(
        content_key = %content_stored.content_key,
        count = classifications.len(),
        "Classified content into scopes"
    );

    // Step 4: Publish brain.scope.assign for each scope match
    for classification in &classifications {
        if classification.relevance < 0.1 {
            continue; // Skip very low relevance
        }

        let assign = ScopeAssignRequest {
            target_key: content_stored.content_key.clone(),
            scope_key: classification.scope_key.clone(),
            relevance: classification.relevance,
        };

        let assign_envelope = EventEnvelope::new(
            "brain.scope.assign",
            PLUGIN_ID,
            correlation_id.clone(),
            scope.clone(),
            &assign,
        )?;

        nats.publish_bytes("brain.scope.assign", serde_json::to_vec(&assign_envelope)?)
            .await
            .context("Failed to publish brain.scope.assign")?;

        info!(
            scope_key = %classification.scope_key,
            relevance = classification.relevance,
            "Published scope assignment"
        );
    }

    Ok(())
}

async fn classify_via_llm(
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    text: &str,
    scopes_json: &str,
) -> Result<Vec<ScopeClassification>> {
    let prompt = format!(
        r#"Classify the following content into the most relevant scopes. Each scope has a key, name, and description.

Available scopes:
{scopes_json}

For each matching scope, provide a relevance score between 0.0 and 1.0 (1.0 = perfectly relevant, 0.0 = not relevant at all). Only include scopes with relevance >= 0.1.

Return ONLY a valid JSON array, no other text. If no scopes match, return [].

Example output:
[
  {{"scope_key": "scope_projects", "relevance": 0.9}},
  {{"scope_key": "scope_job_search", "relevance": 0.3}}
]

Content to classify:
{text}"#
    );

    let request_body = GeminiRequest {
        contents: vec![GeminiContent {
            role: "user".to_string(),
            parts: vec![GeminiPart { text: prompt }],
        }],
        generation_config: GeminiGenerationConfig {
            max_output_tokens: 2048,
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

    let classifications: Vec<ScopeClassification> = serde_json::from_str(json_str)
        .context("Failed to parse LLM scope classification output")?;

    Ok(classifications)
}
