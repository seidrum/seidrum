use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use seidrum_common::events::{
    AgentContextLoaded, BrainQueryRequest, BrainQueryResponse, ChannelInbound, EventEnvelope,
    PluginRegister, SkillSearchRequest, SkillSearchResponse,
};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// CLI arguments
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "seidrum-graph-context-loader",
    about = "Seidrum graph context loader plugin"
)]
struct Cli {
    /// NATS server URL
    #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4222")]
    nats_url: String,

    /// Graph traversal depth (hops)
    #[arg(long, env = "GRAPH_DEPTH", default_value = "3")]
    graph_depth: u32,

    /// Maximum number of facts to include
    #[arg(long, env = "MAX_FACTS", default_value = "50")]
    max_facts: u32,

    /// Minimum fact confidence threshold
    #[arg(long, env = "MIN_CONFIDENCE", default_value = "0.5")]
    min_confidence: f64,

    /// Number of recent conversation messages to include
    #[arg(long, env = "HISTORY_LENGTH", default_value = "20")]
    history_length: u32,

    /// OpenAI API key for embedding generation
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

// ---------------------------------------------------------------------------
// Brain query helpers
// ---------------------------------------------------------------------------

/// Send a brain query request via NATS request/reply and parse the response.
async fn brain_query(
    nats: &seidrum_common::bus_client::BusClient,
    request: &BrainQueryRequest,
) -> Result<BrainQueryResponse> {
    let envelope = EventEnvelope::new(
        "brain.query.request",
        "graph-context-loader",
        None,
        None,
        request,
    )?;
    let payload = serde_json::to_vec(&envelope)?;

    let reply = nats
        .request_bytes("brain.query.request", payload)
        .await
        .context("brain.query.request NATS request failed")?;

    let reply_envelope: EventEnvelope =
        serde_json::from_slice(&reply).context("failed to parse brain query reply envelope")?;

    let response: BrainQueryResponse = serde_json::from_value(reply_envelope.payload)
        .context("failed to parse BrainQueryResponse from reply payload")?;

    Ok(response)
}

/// Request vector-similar content from the brain using an embedding.
async fn query_vector_search(
    nats: &seidrum_common::bus_client::BusClient,
    embedding: &[f64],
    limit: u32,
    user_id: Option<String>,
) -> Result<BrainQueryResponse> {
    let request = BrainQueryRequest {
        query_type: "vector_search".to_string(),
        aql: None,
        bind_vars: None,
        embedding: Some(embedding.to_vec()),
        collection: Some("content".to_string()),
        limit: Some(limit),
        start_vertex: None,
        direction: None,
        depth: None,
        query_text: None,
        max_facts: None,
        graph_depth: None,
        min_confidence: None,
        user_id,
    };
    brain_query(nats, &request).await
}

/// Request high-level context from the brain (entities, facts, tasks).
async fn query_get_context(
    nats: &seidrum_common::bus_client::BusClient,
    text: &str,
    graph_depth: u32,
    max_facts: u32,
    min_confidence: f64,
    user_id: Option<String>,
) -> Result<BrainQueryResponse> {
    let request = BrainQueryRequest {
        query_type: "get_context".to_string(),
        aql: None,
        bind_vars: None,
        embedding: None,
        collection: None,
        limit: None,
        start_vertex: None,
        direction: None,
        depth: None,
        query_text: Some(text.to_string()),
        max_facts: Some(max_facts),
        graph_depth: Some(graph_depth),
        min_confidence: Some(min_confidence),
        user_id,
    };
    brain_query(nats, &request).await
}

/// Request recent conversation history from the brain.
async fn query_conversation_history(
    nats: &seidrum_common::bus_client::BusClient,
    chat_id: &str,
    limit: u32,
    user_id: Option<String>,
) -> Result<BrainQueryResponse> {
    let mut bind_vars = std::collections::HashMap::new();
    bind_vars.insert(
        "chat_id".to_string(),
        serde_json::Value::String(chat_id.to_string()),
    );
    bind_vars.insert(
        "limit".to_string(),
        serde_json::Value::Number(serde_json::Number::from(limit)),
    );

    // Scope conversation history to the requesting user when available
    let aql = if user_id.is_some() {
        "FOR c IN content \
         FILTER c.channel_id == @chat_id \
         FILTER c.user_id == @user_id \
         SORT c.timestamp DESC \
         LIMIT @limit \
         RETURN c"
    } else {
        "FOR c IN content \
         FILTER c.channel_id == @chat_id \
         SORT c.timestamp DESC \
         LIMIT @limit \
         RETURN c"
    };

    if let Some(ref uid) = user_id {
        bind_vars.insert(
            "user_id".to_string(),
            serde_json::Value::String(uid.clone()),
        );
    }

    let request = BrainQueryRequest {
        query_type: "aql".to_string(),
        aql: Some(aql.to_string()),
        bind_vars: Some(bind_vars),
        embedding: None,
        collection: None,
        limit: Some(limit),
        start_vertex: None,
        direction: None,
        depth: None,
        query_text: None,
        max_facts: None,
        graph_depth: None,
        min_confidence: None,
        user_id,
    };
    brain_query(nats, &request).await
}

/// Request active tasks from the brain, scoped to user when available.
async fn query_active_tasks(
    nats: &seidrum_common::bus_client::BusClient,
    user_id: Option<String>,
) -> Result<BrainQueryResponse> {
    let aql = if user_id.is_some() {
        "FOR t IN tasks \
         FILTER t.status IN ['pending', 'in_progress'] \
         FILTER t.user_id == @user_id \
         SORT t.priority == 'high' ? 0 : t.priority == 'medium' ? 1 : 2 \
         RETURN t"
    } else {
        "FOR t IN tasks \
         FILTER t.status IN ['pending', 'in_progress'] \
         SORT t.priority == 'high' ? 0 : t.priority == 'medium' ? 1 : 2 \
         RETURN t"
    };

    let bind_vars = user_id.as_ref().map(|uid| {
        let mut m = std::collections::HashMap::new();
        m.insert(
            "user_id".to_string(),
            serde_json::Value::String(uid.clone()),
        );
        m
    });

    let request = BrainQueryRequest {
        query_type: "aql".to_string(),
        aql: Some(aql.to_string()),
        bind_vars,
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
        user_id,
    };
    brain_query(nats, &request).await
}

/// Search for relevant skills via the brain.skill.search NATS endpoint.
async fn query_skill_search(
    nats: &seidrum_common::bus_client::BusClient,
    query: &str,
    scope: Option<&str>,
) -> Result<SkillSearchResponse> {
    let request = SkillSearchRequest {
        query: query.to_string(),
        limit: Some(5),
        scope: scope.map(|s| s.to_string()),
    };

    let envelope = EventEnvelope::new(
        "brain.skill.search",
        "graph-context-loader",
        None,
        None,
        &request,
    )?;
    let payload = serde_json::to_vec(&envelope)?;

    let reply = nats
        .request_bytes("brain.skill.search", payload)
        .await
        .context("brain.skill.search NATS request failed")?;

    let reply_envelope: EventEnvelope =
        serde_json::from_slice(&reply).context("failed to parse skill search reply envelope")?;

    let response: SkillSearchResponse = serde_json::from_value(reply_envelope.payload)
        .context("failed to parse SkillSearchResponse from reply payload")?;

    Ok(response)
}

// ---------------------------------------------------------------------------
// Context assembly
// ---------------------------------------------------------------------------

/// Extract entities, facts, similar content, tasks, and history from brain
/// query responses and assemble an `AgentContextLoaded` event.
fn assemble_context(
    original_event: EventEnvelope,
    context_resp: Option<BrainQueryResponse>,
    vector_resp: Option<BrainQueryResponse>,
    tasks_resp: Option<BrainQueryResponse>,
    history_resp: Option<BrainQueryResponse>,
    skill_snippets: Vec<serde_json::Value>,
) -> AgentContextLoaded {
    // The get_context response is expected to contain entities and facts.
    let (entities, facts) = match context_resp {
        Some(resp) => {
            let results = resp.results;
            let entities = results
                .get("entities")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let facts = results
                .get("facts")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            (entities, facts)
        }
        None => (Vec::new(), Vec::new()),
    };

    let similar_content = match vector_resp {
        Some(resp) => resp.results.as_array().cloned().unwrap_or_default(),
        None => Vec::new(),
    };

    let active_tasks = match tasks_resp {
        Some(resp) => resp.results.as_array().cloned().unwrap_or_default(),
        None => Vec::new(),
    };

    let conversation_history = match history_resp {
        Some(resp) => resp.results.as_array().cloned().unwrap_or_default(),
        None => Vec::new(),
    };

    AgentContextLoaded {
        original_event,
        entities,
        facts,
        similar_content,
        active_tasks,
        conversation_history,
        skill_snippets,
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    let has_api_key = !cli.openai_api_key.is_empty();
    if !has_api_key {
        warn!("No OPENAI_API_KEY provided; vector search will use text-only queries");
    }

    info!(
        nats_url = %cli.nats_url,
        graph_depth = cli.graph_depth,
        max_facts = cli.max_facts,
        min_confidence = cli.min_confidence,
        history_length = cli.history_length,
        has_api_key,
        "Starting seidrum-graph-context-loader plugin..."
    );

    // Connect to NATS
    let nats =
        seidrum_common::bus_client::BusClient::connect(&cli.nats_url, "graph-context-loader")
            .await
            .context("failed to connect to NATS")?;
    info!("Connected to NATS");

    // Register with kernel
    let register = PluginRegister {
        id: "graph-context-loader".to_string(),
        name: "Graph Context Loader".to_string(),
        version: "0.1.0".to_string(),
        description: "Loads graph context for incoming messages".to_string(),
        consumes: vec!["channel.*.inbound".to_string()],
        produces: vec!["agent.context.loaded".to_string()],
        health_subject: "plugin.graph-context-loader.health".to_string(),
        consumed_event_types: vec![],
        produced_event_types: vec![],
        config_schema: None,
    };

    let register_envelope = EventEnvelope::new(
        "plugin.register",
        "graph-context-loader",
        None,
        None,
        &register,
    )?;
    let register_bytes = serde_json::to_vec(&register_envelope)?;
    nats.publish_bytes("plugin.register", register_bytes)
        .await
        .context("failed to publish plugin.register")?;
    info!("Published plugin.register");

    // Subscribe to channel.*.inbound
    let mut sub = nats
        .subscribe("channel.*.inbound")
        .await
        .context("failed to subscribe to channel.*.inbound")?;
    info!("Subscribed to channel.*.inbound");

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;

    while let Some(msg) = sub.next().await {
        let subject = msg.subject.to_string();

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
            platform = %inbound.platform,
            user_id = %inbound.user_id,
            text_len = inbound.text.len(),
            "Processing channel inbound event for context loading"
        );

        // Step 1: Generate embedding for vector search (if API key available)
        let embedding = if has_api_key {
            match generate_embedding(&http_client, &cli.openai_api_key, &inbound.text).await {
                Ok(emb) => {
                    info!(
                        event_id = %envelope.id,
                        dims = emb.len(),
                        "Generated embedding for vector search"
                    );
                    Some(emb)
                }
                Err(err) => {
                    warn!(
                        %err,
                        event_id = %envelope.id,
                        "Failed to generate embedding; skipping vector search"
                    );
                    None
                }
            }
        } else {
            None
        };

        let user_id = if inbound.user_id.is_empty() {
            None
        } else {
            Some(inbound.user_id.clone())
        };

        // Step 2: Query brain for vector-similar content
        let vector_resp = if let Some(ref emb) = embedding {
            match query_vector_search(&nats, emb, 10, user_id.clone()).await {
                Ok(resp) => {
                    info!(
                        event_id = %envelope.id,
                        count = resp.count,
                        duration_ms = resp.duration_ms,
                        "Vector search returned results"
                    );
                    Some(resp)
                }
                Err(err) => {
                    warn!(
                        %err,
                        event_id = %envelope.id,
                        "Vector search failed"
                    );
                    None
                }
            }
        } else {
            None
        };

        // Step 3: Query brain for get_context (entities, facts via graph traversal)
        let context_resp = match query_get_context(
            &nats,
            &inbound.text,
            cli.graph_depth,
            cli.max_facts,
            cli.min_confidence,
            user_id.clone(),
        )
        .await
        {
            Ok(resp) => {
                info!(
                    event_id = %envelope.id,
                    count = resp.count,
                    duration_ms = resp.duration_ms,
                    "Get context returned results"
                );
                Some(resp)
            }
            Err(err) => {
                warn!(
                    %err,
                    event_id = %envelope.id,
                    "Get context query failed"
                );
                None
            }
        };

        // Step 4: Query brain for active tasks (scoped to user)
        let tasks_resp = match query_active_tasks(&nats, user_id.clone()).await {
            Ok(resp) => {
                info!(
                    event_id = %envelope.id,
                    count = resp.count,
                    duration_ms = resp.duration_ms,
                    "Active tasks query returned results"
                );
                Some(resp)
            }
            Err(err) => {
                warn!(
                    %err,
                    event_id = %envelope.id,
                    "Active tasks query failed"
                );
                None
            }
        };

        // Step 5: Query brain for recent conversation history (scoped to user)
        let history_resp = match query_conversation_history(
            &nats,
            &inbound.chat_id,
            cli.history_length,
            user_id.clone(),
        )
        .await
        {
            Ok(resp) => {
                info!(
                    event_id = %envelope.id,
                    count = resp.count,
                    duration_ms = resp.duration_ms,
                    "Conversation history query returned results"
                );
                Some(resp)
            }
            Err(err) => {
                warn!(
                    %err,
                    event_id = %envelope.id,
                    "Conversation history query failed"
                );
                None
            }
        };

        // Step 6: Search for relevant skills
        let skill_snippets = match tokio::time::timeout(
            Duration::from_secs(5),
            query_skill_search(&nats, &inbound.text, envelope.scope.as_deref()),
        )
        .await
        {
            Ok(Ok(resp)) => {
                info!(
                    event_id = %envelope.id,
                    count = resp.skills.len(),
                    "Skill search returned results"
                );
                resp.skills
                    .into_iter()
                    .filter_map(|s| serde_json::to_value(s).ok())
                    .collect()
            }
            Ok(Err(err)) => {
                warn!(
                    %err,
                    event_id = %envelope.id,
                    "Skill search failed; continuing without skills"
                );
                Vec::new()
            }
            Err(_) => {
                warn!(
                    event_id = %envelope.id,
                    "Skill search timed out; continuing without skills"
                );
                Vec::new()
            }
        };

        // Step 7: Assemble AgentContextLoaded
        let context_loaded = assemble_context(
            envelope.clone(),
            context_resp,
            vector_resp,
            tasks_resp,
            history_resp,
            skill_snippets,
        );

        // Step 8: Publish agent.context.loaded
        let context_envelope = EventEnvelope::new(
            "agent.context.loaded",
            "graph-context-loader",
            envelope.correlation_id.clone(),
            envelope.scope.clone(),
            &context_loaded,
        )?;
        let context_bytes = serde_json::to_vec(&context_envelope)?;

        if let Err(err) = nats
            .publish_bytes("agent.context.loaded", context_bytes)
            .await
        {
            error!(
                %err,
                event_id = %envelope.id,
                "Failed to publish agent.context.loaded"
            );
            continue;
        }

        info!(
            event_id = %envelope.id,
            entities = context_loaded.entities.len(),
            facts = context_loaded.facts.len(),
            similar = context_loaded.similar_content.len(),
            tasks = context_loaded.active_tasks.len(),
            history = context_loaded.conversation_history.len(),
            skills = context_loaded.skill_snippets.len(),
            "Published agent.context.loaded"
        );
    }

    Ok(())
}
