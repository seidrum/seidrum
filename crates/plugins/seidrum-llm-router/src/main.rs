mod context_assembly;
mod tools;

use std::collections::HashMap;
use std::time::Instant;

use anyhow::Result;
use chrono::Utc;
use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use context_assembly::{assemble_context, ContextConfig};
use tools::{GeminiTool, GeminiToolUse, ToolResult};

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "seidrum-llm-router", about = "Seidrum LLM router plugin")]
struct Cli {
    /// NATS server URL
    #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4222")]
    nats_url: String,

    /// Google API key (falls back to OpenClaw auth-profiles.json)
    #[arg(long, env = "GOOGLE_API_KEY")]
    google_api_key: Option<String>,

    /// Model to use
    #[arg(long, env = "LLM_MODEL", default_value = "gemini-2.5-flash")]
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

    /// Maximum number of tool call rounds before forcing a final response
    #[arg(long, env = "LLM_MAX_TOOL_ROUNDS", default_value = "10")]
    max_tool_rounds: u32,

    /// Maximum number of dynamic tools loaded from brain (0 = pinned only)
    #[arg(long, env = "LLM_MAX_DYNAMIC_TOOLS", default_value = "5")]
    max_dynamic_tools: u32,
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
// Gemini API types (reqwest, no SDK)
// ---------------------------------------------------------------------------

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiSystemInstruction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiToolDeclaration>>,
}

#[derive(Serialize, Debug)]
struct GeminiSystemInstruction {
    parts: Vec<GeminiPart>,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
}

/// Gemini tool declaration wrapper.
#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct GeminiToolDeclaration {
    function_declarations: Vec<GeminiFunctionDeclaration>,
}

/// A single function declaration in Gemini format.
#[derive(Serialize, Debug, Clone)]
struct GeminiFunctionDeclaration {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

/// Gemini message content (used for both request and response).
#[derive(Serialize, Deserialize, Debug, Clone)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

/// A part in a Gemini message. Can be text, functionCall, or functionResponse.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_call: Option<GeminiFunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_response: Option<GeminiFunctionResponse>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct GeminiFunctionCall {
    name: String,
    args: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct GeminiFunctionResponse {
    name: String,
    response: serde_json::Value,
}

impl GeminiContent {
    /// Create a simple text message.
    fn text(role: &str, content: &str) -> Self {
        Self {
            role: role.to_string(),
            parts: vec![GeminiPart {
                text: Some(content.to_string()),
                function_call: None,
                function_response: None,
            }],
        }
    }

    /// Extract plain text content from the first text part.
    fn text_content(&self) -> Option<&str> {
        self.parts.iter().find_map(|p| p.text.as_deref())
    }
}

// Gemini API response types

#[derive(Deserialize, Debug)]
struct GeminiResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(rename = "usageMetadata")]
    usage_metadata: Option<GeminiUsageMetadata>,
    #[serde(default)]
    error: Option<GeminiErrorDetail>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct GeminiCandidate {
    content: GeminiContent,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct GeminiUsageMetadata {
    #[serde(default)]
    prompt_token_count: u32,
    #[serde(default)]
    candidates_token_count: u32,
    #[serde(default)]
    total_token_count: u32,
}

#[derive(Deserialize, Debug)]
struct GeminiErrorDetail {
    message: String,
    #[serde(default)]
    code: Option<i32>,
}

// ---------------------------------------------------------------------------
// API key resolution
// ---------------------------------------------------------------------------

/// Resolve the Google API key from env var or OpenClaw auth-profiles.json fallback.
fn resolve_google_api_key(cli_key: &Option<String>) -> Result<String> {
    // 1. CLI arg / env var
    if let Some(key) = cli_key {
        if !key.is_empty() {
            info!("Using Google API key from GOOGLE_API_KEY env var");
            return Ok(key.clone());
        }
    }

    // 2. OpenClaw auth-profiles.json fallback
    let auth_path = dirs::home_dir()
        .map(|h| h.join(".openclaw/agents/main/agent/auth-profiles.json"))
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;

    let content = std::fs::read_to_string(&auth_path)
        .map_err(|e| anyhow::anyhow!("Failed to read OpenClaw auth-profiles.json at {}: {}", auth_path.display(), e))?;

    let profiles: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse OpenClaw auth-profiles.json: {}", e))?;

    let key = profiles
        .get("profiles")
        .and_then(|p| p.get("google:default"))
        .and_then(|v| v.get("key"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("No google:default.key found in OpenClaw auth-profiles.json"))?;

    info!("Using Google API key from OpenClaw auth-profiles.json");
    Ok(key.to_string())
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    info!(nats_url = %cli.nats_url, model = %cli.model, "Starting seidrum-llm-router plugin...");

    // Resolve API key
    let api_key = resolve_google_api_key(&cli.google_api_key)?;

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
    let model = cli.model.clone();
    let max_tokens = cli.max_tokens;
    let max_context_tokens = cli.max_context_tokens;
    let max_tool_rounds = cli.max_tool_rounds;
    let max_dynamic_tools = cli.max_dynamic_tools;

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
                max_tool_rounds,
                max_dynamic_tools,
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
                max_tool_rounds,
                max_dynamic_tools,
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
    max_tool_rounds: u32,
    max_dynamic_tools: u32,
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

            (Some(assembled.system_prompt), assembled.messages, agent_id, user_text)
        } else {
            // Simple path for channel.*.inbound — just the user message
            let inbound: ChannelInbound = serde_json::from_value(envelope.payload)?;
            let agent_id = envelope.scope.unwrap_or_else(|| "default".to_string());

            if inbound.text.is_empty() {
                warn!(subject = %subject, "Empty user text, skipping");
                return Ok(());
            }

            let user_text = inbound.text.clone();
            let msgs = vec![GeminiContent::text("user", &inbound.text)];

            (None, msgs, agent_id, user_text)
        };

    // Collect tools (pinned + dynamic from brain)
    let api_tools = tools::collect_tools(nats, &user_text_for_tools, max_dynamic_tools).await;
    let tools_option = if api_tools.is_empty() {
        None
    } else {
        // Convert GeminiTool list to Gemini tool declaration format
        let function_declarations: Vec<GeminiFunctionDeclaration> = api_tools
            .iter()
            .map(|t| GeminiFunctionDeclaration {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.parameters.clone(),
            })
            .collect();
        Some(vec![GeminiToolDeclaration { function_declarations }])
    };

    // -----------------------------------------------------------------------
    // Tool call loop: call LLM, execute tools, repeat until content or max rounds
    // -----------------------------------------------------------------------
    let mut messages = messages;
    let mut total_input_tokens: u32 = 0;
    let mut total_output_tokens: u32 = 0;
    let mut final_model = model.to_string();
    let mut final_finish_reason = "unknown".to_string();
    let mut final_content: Option<String> = None;
    let mut final_tool_calls: Option<Vec<serde_json::Value>> = None;
    let start = Instant::now();

    for round in 0..=max_tool_rounds {
        info!(round, "LLM call round");

        let system_instruction = system_prompt.as_ref().map(|s| GeminiSystemInstruction {
            parts: vec![GeminiPart {
                text: Some(s.clone()),
                function_call: None,
                function_response: None,
            }],
        });

        let api_request = GeminiRequest {
            contents: messages.clone(),
            system_instruction,
            generation_config: Some(GeminiGenerationConfig {
                temperature: Some(0.7),
                max_output_tokens: Some(max_tokens),
            }),
            tools: tools_option.clone(),
        };

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            model, api_key
        );

        let response = http
            .post(&url)
            .header("content-type", "application/json")
            .json(&api_request)
            .send()
            .await?;

        let status = response.status();
        let body_bytes = response.bytes().await?;

        if !status.is_success() {
            let err_msg = match serde_json::from_slice::<GeminiResponse>(&body_bytes) {
                Ok(resp) => {
                    if let Some(err) = resp.error {
                        format!("code {:?}: {}", err.code, err.message)
                    } else {
                        String::from_utf8_lossy(&body_bytes).to_string()
                    }
                }
                Err(_) => String::from_utf8_lossy(&body_bytes).to_string(),
            };
            anyhow::bail!("Gemini API error ({}): {}", status, err_msg);
        }

        let api_response: GeminiResponse = serde_json::from_slice(&body_bytes)?;

        if let Some(usage) = &api_response.usage_metadata {
            total_input_tokens += usage.prompt_token_count;
            total_output_tokens += usage.candidates_token_count;
        }

        let candidate = api_response.candidates.first()
            .ok_or_else(|| anyhow::anyhow!("No candidates in Gemini response"))?;

        final_finish_reason = candidate
            .finish_reason
            .clone()
            .unwrap_or_else(|| "unknown".to_string());

        info!(
            model = %model,
            input_tokens = total_input_tokens,
            output_tokens = total_output_tokens,
            finish_reason = %final_finish_reason,
            round,
            "Gemini API response received"
        );

        // Extract text content from parts
        let content_text: String = candidate
            .content
            .parts
            .iter()
            .filter_map(|p| p.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n");

        // Extract functionCall parts
        let tool_uses: Vec<GeminiToolUse> = candidate
            .content
            .parts
            .iter()
            .filter_map(|p| {
                p.function_call.as_ref().map(|fc| GeminiToolUse {
                    id: format!("call_{}", generate_ulid()),
                    name: fc.name.clone(),
                    args: fc.args.clone(),
                })
            })
            .collect();

        if tool_uses.is_empty() || round == max_tool_rounds {
            // No tool calls or we've hit max rounds -- we're done
            if round == max_tool_rounds && !tool_uses.is_empty() {
                warn!(
                    max_rounds = max_tool_rounds,
                    "Hit maximum tool call rounds, returning partial content"
                );
            }
            final_content = if content_text.is_empty() {
                None
            } else {
                Some(content_text)
            };
            break;
        }

        // There are tool calls -- execute them and continue the loop
        info!(
            tool_count = tool_uses.len(),
            round, "Executing tool calls"
        );

        // Append the model's response (with functionCall parts) to messages
        messages.push(candidate.content.clone());

        // Execute all tool calls and build functionResponse parts
        let mut response_parts: Vec<GeminiPart> = Vec::new();
        for tool_use in &tool_uses {
            let result = tools::execute_tool_call(tool_use, nats).await;
            response_parts.push(GeminiPart {
                text: None,
                function_call: None,
                function_response: Some(GeminiFunctionResponse {
                    name: tool_use.name.clone(),
                    response: serde_json::json!({
                        "result": result.content,
                        "is_error": result.is_error,
                    }),
                }),
            });
        }

        // Record tool calls for the LlmResponse event
        final_tool_calls = Some(
            tool_uses
                .iter()
                .map(|tu| {
                    serde_json::json!({
                        "id": tu.id,
                        "function_name": tu.name,
                        "arguments": serde_json::to_string(&tu.args).unwrap_or_default()
                    })
                })
                .collect(),
        );

        // Append tool results as a user message with functionResponse parts
        messages.push(GeminiContent {
            role: "user".to_string(),
            parts: response_parts,
        });
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    let total_tokens = total_input_tokens + total_output_tokens;

    // Build llm.response event
    let llm_response = LlmResponse {
        agent_id,
        content: final_content,
        tool_calls: final_tool_calls,
        model_used: final_model,
        provider: "google".to_string(),
        tokens: TokenUsage {
            prompt_tokens: total_input_tokens,
            completion_tokens: total_output_tokens,
            total_tokens,
            estimated_cost_usd: 0.0, // Cost tracking deferred to future iteration
        },
        duration_ms,
        finish_reason: final_finish_reason,
    };

    // Wrap in EventEnvelope
    let out_envelope = EventEnvelope {
        id: generate_ulid(),
        event_type: "llm.response".to_string(),
        timestamp: Utc::now(),
        source: "llm-router".to_string(),
        correlation_id,
        scope: None,
        payload: serde_json::to_value(&llm_response)?,
    };

    let envelope_bytes = serde_json::to_vec(&out_envelope)?;
    nats.publish("llm.response", envelope_bytes.into())
        .await?;

    info!(event_id = %out_envelope.id, "Published llm.response");

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
