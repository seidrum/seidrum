mod context_assembly;

use std::collections::HashMap;
use std::time::Instant;

use anyhow::Result;
use chrono::Utc;
use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use context_assembly::{assemble_context, ContextConfig};

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "seidrum-llm-router", about = "Seidrum LLM router plugin")]
struct Cli {
    /// NATS server URL
    #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4222")]
    nats_url: String,

    /// Anthropic API key
    #[arg(long, env = "ANTHROPIC_API_KEY")]
    anthropic_api_key: String,

    /// Model to use
    #[arg(long, env = "LLM_MODEL", default_value = "claude-sonnet-4-20250514")]
    model: String,

    /// Max tokens for response
    #[arg(long, env = "LLM_MAX_TOKENS", default_value = "4096")]
    max_tokens: u32,

    /// Path to the Tera prompt template file
    #[arg(long, env = "LLM_PROMPT_PATH", default_value = "./prompts/assistant.md")]
    prompt_path: String,

    /// Maximum context window size in tokens
    #[arg(long, env = "LLM_MAX_CONTEXT_TOKENS", default_value = "100000")]
    max_context_tokens: usize,
}

// ---------------------------------------------------------------------------
// Event envelope (matches EVENT_CATALOG.md)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
struct EventEnvelope {
    id: String,
    event_type: String,
    timestamp: chrono::DateTime<Utc>,
    source: String,
    correlation_id: Option<String>,
    scope: Option<String>,
    payload: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Inbound event payloads
// ---------------------------------------------------------------------------

/// Payload for channel.*.inbound events.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct ChannelInbound {
    platform: String,
    user_id: String,
    chat_id: String,
    text: String,
    reply_to: Option<String>,
    #[serde(default)]
    attachments: Vec<serde_json::Value>,
    #[serde(default)]
    metadata: HashMap<String, String>,
}

/// Payload for agent.context.loaded events.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct AgentContextLoaded {
    original_event: EventEnvelope,
    #[serde(default)]
    entities: Vec<serde_json::Value>,
    #[serde(default)]
    facts: Vec<serde_json::Value>,
    #[serde(default)]
    similar_content: Vec<serde_json::Value>,
    #[serde(default)]
    active_tasks: Vec<serde_json::Value>,
    #[serde(default)]
    conversation_history: Vec<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// LLM response event (published to NATS)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
struct LlmResponse {
    agent_id: String,
    content: Option<String>,
    tool_calls: Option<Vec<serde_json::Value>>,
    model_used: String,
    provider: String,
    tokens: TokenUsage,
    duration_ms: u64,
    finish_reason: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct TokenUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
    estimated_cost_usd: f64,
}

// ---------------------------------------------------------------------------
// Anthropic API types (reqwest, no SDK)
// ---------------------------------------------------------------------------

#[derive(Serialize, Debug)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<AnthropicMessage>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct AnthropicMessage {
    role: String,
    content: String,
}

#[derive(Deserialize, Debug)]
struct AnthropicResponse {
    id: String,
    content: Vec<AnthropicContent>,
    model: String,
    stop_reason: Option<String>,
    usage: AnthropicUsage,
}

#[derive(Deserialize, Debug)]
struct AnthropicContent {
    #[serde(rename = "type")]
    content_type: String,
    text: Option<String>,
}

#[derive(Deserialize, Debug)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

#[derive(Deserialize, Debug)]
struct AnthropicErrorResponse {
    error: AnthropicErrorDetail,
}

#[derive(Deserialize, Debug)]
struct AnthropicErrorDetail {
    message: String,
    #[serde(rename = "type")]
    error_type: String,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    info!(nats_url = %cli.nats_url, model = %cli.model, "Starting seidrum-llm-router plugin...");

    // Connect to NATS
    let nats = async_nats::connect(&cli.nats_url).await?;
    info!("Connected to NATS");

    // Publish plugin registration
    let register = serde_json::json!({
        "id": "llm-router",
        "name": "LLM Router",
        "version": "0.1.0",
        "description": "Routes messages to LLM providers and publishes responses",
        "consumes": ["agent.context.loaded", "channel.*.inbound"],
        "produces": ["llm.response"],
        "health_subject": "plugin.llm-router.health",
    });
    nats.publish(
        "plugin.register",
        serde_json::to_vec(&register)?.into(),
    )
    .await?;
    info!("Published plugin.register");

    // Subscribe to both subjects
    let sub_context = nats.subscribe("agent.context.loaded").await?;
    let sub_inbound = nats.subscribe("channel.*.inbound").await?;
    info!("Subscribed to agent.context.loaded and channel.*.inbound");

    // Load prompt template from disk
    let prompt_template = match std::fs::read_to_string(&cli.prompt_path) {
        Ok(content) => {
            info!(path = %cli.prompt_path, "Loaded prompt template");
            content
        }
        Err(e) => {
            warn!(path = %cli.prompt_path, error = %e, "Could not load prompt template, using default");
            "You are {{ user_name }}'s personal assistant.\n\nCurrent time: {{ current_time }}\nContext: {{ scope_name }}\n\n## What you know\n{{ current_facts }}\n\n## Active tasks\n{{ active_tasks }}\n\n## Recent conversation\n{{ conversation_history }}\n\n## Instructions\n- Be direct and concise.\n- If you identify an actionable item, create a task.\n- If you learn a new fact, note it in your response.\n- Stay within your scope.\n".to_string()
        }
    };

    // Build the HTTP client once
    let http = reqwest::Client::new();

    // Merge both subscription streams into a single processing loop
    let nats_pub = nats.clone();
    let api_key = cli.anthropic_api_key.clone();
    let model = cli.model.clone();
    let max_tokens = cli.max_tokens;
    let max_context_tokens = cli.max_context_tokens;

    // We spawn two tasks: one per subscription. Both share the same processing logic.
    let http1 = http.clone();
    let api_key1 = api_key.clone();
    let model1 = model.clone();
    let nats1 = nats_pub.clone();
    let prompt_template1 = prompt_template.clone();

    let handle_context = tokio::spawn(async move {
        let mut sub = sub_context;
        while let Some(msg) = tokio::select! {
            m = futures_next(&mut sub) => m,
        } {
            if let Err(e) = handle_message(
                &msg.payload,
                &msg.subject,
                &http1,
                &api_key1,
                &model1,
                max_tokens,
                max_context_tokens,
                &prompt_template1,
                &nats1,
            )
            .await
            {
                error!(error = %e, subject = %msg.subject, "Failed to process message");
            }
        }
    });

    let handle_inbound = tokio::spawn(async move {
        let mut sub = sub_inbound;
        while let Some(msg) = futures_next(&mut sub).await {
            if let Err(e) = handle_message(
                &msg.payload,
                &msg.subject,
                &http,
                &api_key,
                &model,
                max_tokens,
                max_context_tokens,
                &prompt_template,
                &nats_pub,
            )
            .await
            {
                error!(error = %e, subject = %msg.subject, "Failed to process message");
            }
        }
    });

    // Wait for both tasks (they run forever unless NATS disconnects)
    tokio::select! {
        r = handle_context => { if let Err(e) = r { error!(error = %e, "context handler panicked"); } }
        r = handle_inbound => { if let Err(e) = r { error!(error = %e, "inbound handler panicked"); } }
    }

    Ok(())
}

/// Pull the next message from an async-nats subscriber.
async fn futures_next(
    sub: &mut async_nats::Subscriber,
) -> Option<async_nats::Message> {
    use futures::StreamExt as _;
    sub.next().await
}

// ---------------------------------------------------------------------------
// Message handler
// ---------------------------------------------------------------------------

async fn handle_message(
    payload: &[u8],
    subject: &str,
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    max_tokens: u32,
    max_context_tokens: usize,
    prompt_template: &str,
    nats: &async_nats::Client,
) -> Result<()> {
    info!(subject = %subject, "Received event");

    let envelope: EventEnvelope = serde_json::from_slice(payload)?;
    let correlation_id = envelope
        .correlation_id
        .clone()
        .or_else(|| Some(envelope.id.clone()));

    // Branch based on event type
    let (system_prompt, messages, agent_id) = if subject == "agent.context.loaded" {
        // Full context assembly path
        let ctx: AgentContextLoaded = serde_json::from_value(envelope.payload)?;
        let agent_id = ctx
            .original_event
            .scope
            .clone()
            .unwrap_or_else(|| "default".to_string());

        let config = ContextConfig {
            max_context_tokens,
            max_response_tokens: max_tokens as usize,
            prompt_template: prompt_template.to_string(),
        };

        let assembled = assemble_context(&config, &ctx)?;
        info!(
            estimated_tokens = assembled.estimated_tokens,
            messages = assembled.messages.len(),
            "Context assembled from agent.context.loaded"
        );

        (Some(assembled.system_prompt), assembled.messages, agent_id)
    } else {
        // Simple path for channel.*.inbound — just the user message
        let inbound: ChannelInbound = serde_json::from_value(envelope.payload)?;
        let agent_id = envelope.scope.unwrap_or_else(|| "default".to_string());

        if inbound.text.is_empty() {
            warn!(subject = %subject, "Empty user text, skipping");
            return Ok(());
        }

        let msgs = vec![AnthropicMessage {
            role: "user".to_string(),
            content: inbound.text,
        }];

        (None, msgs, agent_id)
    };

    let user_text_len: usize = messages.iter().map(|m| m.content.len()).sum();
    info!(text_len = user_text_len, "Calling Anthropic API");

    // Build Anthropic API request
    let api_request = AnthropicRequest {
        model: model.to_string(),
        max_tokens,
        system: system_prompt,
        messages,
    };

    let start = Instant::now();

    let response = http
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&api_request)
        .send()
        .await?;

    let duration_ms = start.elapsed().as_millis() as u64;
    let status = response.status();
    let body_bytes = response.bytes().await?;

    if !status.is_success() {
        // Try to parse error body for a better message
        let err_msg = match serde_json::from_slice::<AnthropicErrorResponse>(&body_bytes) {
            Ok(err) => format!("{}: {}", err.error.error_type, err.error.message),
            Err(_) => String::from_utf8_lossy(&body_bytes).to_string(),
        };
        anyhow::bail!("Anthropic API error ({}): {}", status, err_msg);
    }

    let api_response: AnthropicResponse = serde_json::from_slice(&body_bytes)?;

    // Extract text content from the response
    let content_text: String = api_response
        .content
        .iter()
        .filter(|c| c.content_type == "text")
        .filter_map(|c| c.text.as_deref())
        .collect::<Vec<_>>()
        .join("\n");

    info!(
        model = %api_response.model,
        input_tokens = api_response.usage.input_tokens,
        output_tokens = api_response.usage.output_tokens,
        duration_ms = duration_ms,
        "Anthropic API response received"
    );

    // Build llm.response event
    let total_tokens = api_response.usage.input_tokens + api_response.usage.output_tokens;
    let llm_response = LlmResponse {
        agent_id,
        content: if content_text.is_empty() {
            None
        } else {
            Some(content_text)
        },
        tool_calls: None,
        model_used: api_response.model,
        provider: "anthropic".to_string(),
        tokens: TokenUsage {
            prompt_tokens: api_response.usage.input_tokens,
            completion_tokens: api_response.usage.output_tokens,
            total_tokens,
            estimated_cost_usd: 0.0, // Cost tracking deferred to future iteration
        },
        duration_ms,
        finish_reason: api_response
            .stop_reason
            .unwrap_or_else(|| "unknown".to_string()),
    };

    // Wrap in EventEnvelope
    let envelope = EventEnvelope {
        id: generate_ulid(),
        event_type: "llm.response".to_string(),
        timestamp: Utc::now(),
        source: "llm-router".to_string(),
        correlation_id,
        scope: None,
        payload: serde_json::to_value(&llm_response)?,
    };

    let envelope_bytes = serde_json::to_vec(&envelope)?;
    nats.publish("llm.response", envelope_bytes.into())
        .await?;

    info!(event_id = %envelope.id, "Published llm.response");

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Generate a simple time-based unique ID (ULID-like).
/// Uses timestamp + random suffix. A proper ULID crate can be added later.
fn generate_ulid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let rand_part: u64 = rand_u64();
    format!("{:012x}-{:016x}", ts, rand_part)
}

/// Simple pseudo-random u64 using thread-local state seeded from the clock.
fn rand_u64() -> u64 {
    use std::cell::Cell;
    use std::time::{SystemTime, UNIX_EPOCH};
    thread_local! {
        static STATE: Cell<u64> = Cell::new(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
        );
    }
    STATE.with(|s| {
        // xorshift64
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        x
    })
}
