mod context_assembly;
mod routing;
mod tools;

use std::collections::HashMap;
use std::time::Instant;

use anyhow::Result;
use chrono::Utc;
use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use seidrum_common::events::{
    LlmCallConfig, LlmResponse, TokenUsage, UnifiedLlmRequest, UnifiedMessage,
};

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

    /// Default LLM provider to use
    #[arg(long, env = "LLM_PROVIDER", default_value = "google")]
    provider: String,

    /// Path to the Tera prompt template file
    #[arg(
        long,
        env = "LLM_PROMPT_PATH",
        default_value = "./prompts/assistant.md"
    )]
    prompt_path: String,

    /// Maximum context window size in tokens
    #[arg(long, env = "LLM_MAX_CONTEXT_TOKENS", default_value = "100000")]
    max_context_tokens: usize,

    /// Max tokens for response
    #[arg(long, env = "LLM_MAX_TOKENS", default_value = "4096")]
    max_tokens: u32,

    /// Maximum number of dynamic tools loaded from registry (0 = meta only)
    #[arg(long, env = "LLM_MAX_DYNAMIC_TOOLS", default_value = "5")]
    max_dynamic_tools: u32,

    /// Timeout in seconds for the LLM provider request
    #[arg(long, env = "LLM_PROVIDER_TIMEOUT", default_value = "120")]
    provider_timeout: u64,

    /// Routing strategy: "fixed", "fallback", or "intelligent"
    #[arg(long, env = "LLM_ROUTING_STRATEGY", default_value = "fallback")]
    routing_strategy: String,

    /// Comma-separated list of fallback providers
    #[arg(
        long,
        env = "LLM_FALLBACK_PROVIDERS",
        default_value = "google,openai,anthropic"
    )]
    fallback_providers: String,
}

// ---------------------------------------------------------------------------
// Event envelope (matches EVENT_CATALOG.md)
// ---------------------------------------------------------------------------

/// Origin channel info for response routing.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct EventOrigin {
    platform: String,
    chat_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct EventEnvelope {
    id: String,
    event_type: String,
    timestamp: chrono::DateTime<Utc>,
    source: String,
    correlation_id: Option<String>,
    scope: Option<String>,
    payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<EventOrigin>,
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
    #[serde(default)]
    skill_snippets: Vec<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    info!(
        nats_url = %cli.nats_url,
        provider = %cli.provider,
        "Starting seidrum-llm-router plugin..."
    );

    // Connect to NATS
    let nats = seidrum_common::bus_client::BusClient::connect(&cli.nats_url, "llm-router").await?;
    info!("Connected to NATS");

    // Publish plugin registration
    let register = serde_json::json!({
        "id": "llm-router",
        "name": "LLM Router",
        "version": "0.2.0",
        "description": "Provider-agnostic LLM router — assembles context, queries tool registry, dispatches to provider plugins",
        "consumes": ["agent.context.loaded", "channel.*.inbound"],
        "produces": ["llm.response"],
        "health_subject": "plugin.llm-router.health",
    });
    nats.publish_bytes("plugin.register", serde_json::to_vec(&register)?)
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
            "You are {{ user_name }}'s personal assistant.\n\nCurrent time: {{ current_time }}\nContext: {{ scope_name }}\n\n## What you know\n{{ current_facts }}\n\n## Active tasks\n{{ active_tasks }}\n\n## Recent conversation\n{{ conversation_history }}\n\n{% if active_skills %}\n## Behavioral guidance\n{{ active_skills }}\n{% endif %}\n\n## Instructions\n- Be direct and concise.\n- If you identify an actionable item, create a task.\n- If you learn a new fact, note it in your response.\n- Stay within your scope.\n".to_string()
        }
    };

    // Parse routing configuration
    let routing_config = build_routing_config(&cli.routing_strategy, &cli.fallback_providers)?;
    info!(routing_strategy = %cli.routing_strategy, "Routing configuration loaded");

    // Shared state
    let nats_pub = nats.clone();
    let max_tokens = cli.max_tokens;
    let max_context_tokens = cli.max_context_tokens;
    let max_dynamic_tools = cli.max_dynamic_tools;
    let provider_timeout = cli.provider_timeout;

    // Spawn two tasks: one per subscription
    let nats1 = nats_pub.clone();
    let prompt_template1 = prompt_template.clone();
    let routing_config1 = routing_config.clone();

    let handle_context = tokio::spawn(async move {
        let mut sub = sub_context;
        while let Some(msg) = tokio::select! {
            m = futures_next(&mut sub) => m,
        } {
            if let Err(e) = handle_message(
                &msg.payload,
                &msg.subject,
                max_tokens,
                max_context_tokens,
                max_dynamic_tools,
                provider_timeout,
                &prompt_template1,
                &nats1,
                &routing_config1,
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
                max_tokens,
                max_context_tokens,
                max_dynamic_tools,
                provider_timeout,
                &prompt_template,
                &nats_pub,
                &routing_config,
            )
            .await
            {
                error!(error = %e, subject = %msg.subject, "Failed to process message");
            }
        }
    });

    // Wait for both tasks
    tokio::select! {
        r = handle_context => { if let Err(e) = r { error!(error = %e, "context handler panicked"); } }
        r = handle_inbound => { if let Err(e) = r { error!(error = %e, "inbound handler panicked"); } }
    }

    Ok(())
}

/// Pull the next message from an async-nats subscriber.
async fn futures_next(
    sub: &mut seidrum_common::bus_client::Subscription,
) -> Option<seidrum_common::bus_client::Message> {
    sub.next().await
}

// ---------------------------------------------------------------------------
// Message handler
// ---------------------------------------------------------------------------

async fn handle_message(
    payload: &[u8],
    subject: &str,
    max_tokens: u32,
    max_context_tokens: usize,
    max_dynamic_tools: u32,
    provider_timeout: u64,
    prompt_template: &str,
    nats: &seidrum_common::bus_client::BusClient,
    routing_config: &routing::RoutingStrategy,
) -> Result<()> {
    info!(subject = %subject, "Received event");

    let envelope: EventEnvelope = serde_json::from_slice(payload)?;
    let correlation_id = envelope
        .correlation_id
        .clone()
        .or_else(|| Some(envelope.id.clone()));
    let scope = envelope.scope.clone();

    // Branch based on event type
    let (system_prompt, messages, agent_id, user_text_for_tools) =
        if subject == "agent.context.loaded" {
            // Full context assembly path
            let ctx: AgentContextLoaded = serde_json::from_value(envelope.payload)?;
            let agent_id = ctx
                .original_event
                .scope
                .clone()
                .unwrap_or_else(|| "default".to_string());

            // Extract user text for tool relevance matching
            let user_text = ctx
                .original_event
                .payload
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

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

            // Convert assembled messages to UnifiedMessage format
            let unified_msgs: Vec<UnifiedMessage> = assembled
                .messages
                .iter()
                .map(|m| UnifiedMessage {
                    role: m.role.clone(),
                    content: Some(m.content.clone()),
                    tool_calls: None,
                    tool_results: None,
                })
                .collect();

            (
                Some(assembled.system_prompt),
                unified_msgs,
                agent_id,
                user_text,
            )
        } else {
            // Simple path for channel.*.inbound -- just the user message
            let inbound: ChannelInbound = serde_json::from_value(envelope.payload)?;
            let agent_id = scope.clone().unwrap_or_else(|| "default".to_string());

            if inbound.text.is_empty() {
                warn!(subject = %subject, "Empty user text, skipping");
                return Ok(());
            }

            let user_text = inbound.text.clone();
            let msgs = vec![UnifiedMessage {
                role: "user".to_string(),
                content: Some(inbound.text),
                tool_calls: None,
                tool_results: None,
            }];

            (None, msgs, agent_id, user_text)
        };

    // Query tool registry for available tools
    let mut tool_schemas = tools::meta_tools();
    let registry_tools =
        tools::query_tool_registry(nats, &user_text_for_tools, max_dynamic_tools).await;
    // Deduplicate: only add registry tools whose names don't collide with meta tools (O(n))
    let meta_names: std::collections::HashSet<String> =
        tool_schemas.iter().map(|t| t.name.clone()).collect();
    for t in registry_tools {
        if !meta_names.contains(&t.name) {
            tool_schemas.push(t);
        }
    }

    info!(
        tool_count = tool_schemas.len(),
        "Tools collected for LLM request"
    );

    // Build RequestProfile for intelligent routing
    let estimated_tokens = if let Some(ref sys_prompt) = system_prompt {
        context_assembly::count_tokens(sys_prompt)
            + messages
                .iter()
                .map(|m| {
                    context_assembly::count_tokens(&m.content.as_ref().unwrap_or(&String::new()))
                        + 4
                })
                .sum::<usize>()
    } else {
        messages
            .iter()
            .map(|m| {
                context_assembly::count_tokens(&m.content.as_ref().unwrap_or(&String::new())) + 4
            })
            .sum::<usize>()
    };

    let request_profile = routing::RequestProfile {
        tool_count: tool_schemas.len(),
        message_count: messages.len(),
        estimated_tokens,
        has_system_prompt: system_prompt.is_some(),
        scope: scope.clone(),
        routing_strategy: "best-first".to_string(),
        model_preferences: vec![],
    };

    // Build UnifiedLlmRequest
    let unified_request = UnifiedLlmRequest {
        agent_id: agent_id.clone(),
        messages,
        system_prompt,
        tools: tool_schemas,
        config: LlmCallConfig {
            temperature: Some(0.7),
            max_tokens: Some(max_tokens),
            top_p: None,
        },
        routing_strategy: "best-first".to_string(),
        model_preferences: vec![],
        correlation_id: correlation_id.clone(),
        scope: scope.clone(),
        user_id: None,
    };

    let request_bytes = serde_json::to_vec(&unified_request)?;

    // Get list of providers to try (in order)
    let providers_to_try = get_providers_to_try(&request_profile, routing_config);

    info!(
        providers = ?providers_to_try.iter().map(|p| p.name()).collect::<Vec<_>>(),
        estimated_tokens,
        "Routing to providers"
    );

    let mut last_error: Option<String> = None;
    let mut llm_response: Option<LlmResponse> = None;
    let mut selected_provider: Option<routing::Provider> = None;

    // Try each provider in order
    for provider in &providers_to_try {
        let subject = provider.nats_subject();

        debug!(
            provider = %provider.name(),
            subject = %subject,
            "Attempting provider"
        );

        let start = Instant::now();

        let provider_response = tokio::time::timeout(
            std::time::Duration::from_secs(provider_timeout),
            nats.request_bytes(subject.clone(), request_bytes.clone()),
        )
        .await;

        let duration_ms = start.elapsed().as_millis() as u64;

        match provider_response {
            Ok(Ok(resp_msg)) => match serde_json::from_slice::<LlmResponse>(&resp_msg) {
                Ok(resp) => {
                    info!(
                        provider = %provider.name(),
                        model = %resp.model_used,
                        duration_ms,
                        tokens = resp.tokens.total_tokens,
                        "LLM provider response received"
                    );
                    selected_provider = Some(provider.clone());
                    llm_response = Some(resp);
                    break;
                }
                Err(e) => {
                    let err_msg = format!(
                        "Provider {} failed to parse response: {}",
                        provider.name(),
                        e
                    );
                    warn!(error = %e, provider = %provider.name(), "Failed to parse response");
                    last_error = Some(err_msg);
                }
            },
            Ok(Err(e)) => {
                let err_msg = format!("Provider {} request failed: {}", provider.name(), e);
                warn!(error = %e, provider = %provider.name(), "NATS request failed");
                last_error = Some(err_msg);
            }
            Err(_) => {
                let err_msg = format!("Provider {} request timed out", provider.name());
                warn!(provider = %provider.name(), timeout_secs = provider_timeout, "Request timed out");
                last_error = Some(err_msg);
            }
        }
    }

    // If no provider succeeded, return error response
    let llm_response = llm_response.unwrap_or_else(|| {
        let provider_name = selected_provider
            .as_ref()
            .map(|p| p.name())
            .unwrap_or("unknown");
        let error_msg = last_error.unwrap_or_else(|| "All providers failed".to_string());

        error!("All provider attempts failed: {}", error_msg);

        LlmResponse {
            agent_id: agent_id.clone(),
            content: Some(format!("Error: {}", error_msg)),
            tool_calls: None,
            model_used: "unknown".to_string(),
            provider: provider_name.to_string(),
            tokens: TokenUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                estimated_cost_usd: 0.0,
            },
            duration_ms: 0,
            finish_reason: "error".to_string(),
            tool_rounds: 0,
        }
    });

    // Publish llm.response event (same format as before for downstream compatibility)
    let out_envelope = EventEnvelope {
        id: generate_ulid(),
        event_type: "llm.response".to_string(),
        timestamp: Utc::now(),
        source: "llm-router".to_string(),
        correlation_id,
        scope,
        payload: serde_json::to_value(&llm_response)?,
        origin: envelope.origin.clone(),
    };

    let envelope_bytes = serde_json::to_vec(&out_envelope)?;
    nats.publish_bytes("llm.response", envelope_bytes).await?;

    info!(event_id = %out_envelope.id, "Published llm.response");

    Ok(())
}

// ---------------------------------------------------------------------------
// Routing helpers
// ---------------------------------------------------------------------------

/// Parse routing configuration from CLI arguments.
fn build_routing_config(strategy: &str, fallback_str: &str) -> Result<routing::RoutingStrategy> {
    match strategy {
        "fixed" => Ok(routing::RoutingStrategy::Fixed(routing::Provider::Google)),
        "fallback" => {
            let providers: Result<Vec<routing::Provider>> = fallback_str
                .split(',')
                .map(|s| parse_provider(s.trim()))
                .collect();
            Ok(routing::RoutingStrategy::Fallback(providers?))
        }
        "intelligent" => Ok(routing::RoutingStrategy::Intelligent(
            routing::IntelligentRoutingConfig::default(),
        )),
        other => Err(anyhow::anyhow!(
            "Unknown routing strategy: {}. Use 'fixed', 'fallback', or 'intelligent'",
            other
        )),
    }
}

/// Parse a provider name string into a Provider enum.
fn parse_provider(name: &str) -> Result<routing::Provider> {
    match name.to_lowercase().as_str() {
        "google" | "gemini" => Ok(routing::Provider::Google),
        "openai" | "gpt" => Ok(routing::Provider::OpenAI),
        "anthropic" | "claude" => Ok(routing::Provider::Anthropic),
        "ollama" | "local" => Ok(routing::Provider::Ollama),
        other => Err(anyhow::anyhow!(
            "Unknown provider: {}. Use 'google', 'openai', 'anthropic', or 'ollama'",
            other
        )),
    }
}

/// Get the list of providers to try in order based on routing strategy.
fn get_providers_to_try(
    profile: &routing::RequestProfile,
    config: &routing::RoutingStrategy,
) -> Vec<routing::Provider> {
    match config {
        routing::RoutingStrategy::Fixed(p) => vec![p.clone()],
        routing::RoutingStrategy::Fallback(providers) => providers.clone(),
        routing::RoutingStrategy::Intelligent(ic) => {
            let primary = routing::select_provider(profile, config);
            let mut list = vec![primary.clone()];

            // Add fallback provider if different
            if ic.fallback != primary {
                list.push(ic.fallback.clone());
            }

            // Add any other providers not already in the list for deep fallback
            for other in &[
                routing::Provider::Google,
                routing::Provider::OpenAI,
                routing::Provider::Anthropic,
                routing::Provider::Ollama,
            ] {
                if !list.contains(other) {
                    list.push(other.clone());
                }
            }

            list
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Generate a simple time-based unique ID (ULID-like).
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
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        x
    })
}
