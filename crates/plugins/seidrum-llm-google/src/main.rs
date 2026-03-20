mod gemini_types;
mod translator;

use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use futures::StreamExt as _;
use tracing::{error, info, warn};

use seidrum_common::events::{
    EventEnvelope, LlmResponse, PluginRegister, ToolCallResponse,
    TokenUsage, UnifiedLlmRequest,
};

use gemini_types::{
    GeminiContent, GeminiGenerationConfig, GeminiPart, GeminiRequest, GeminiResponse,
    GeminiSystemInstruction,
};
use translator::{
    gemini_function_calls_to_tool_calls, tool_call_to_dispatch_request,
    unified_to_gemini_contents, unified_to_gemini_tools,
};

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "seidrum-llm-google",
    about = "Seidrum Google Gemini LLM provider plugin"
)]
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

    /// Maximum number of tool call rounds before forcing a final response
    #[arg(long, env = "LLM_MAX_TOOL_ROUNDS", default_value = "10")]
    max_tool_rounds: u32,
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

    let content = std::fs::read_to_string(&auth_path).map_err(|e| {
        anyhow::anyhow!(
            "Failed to read OpenClaw auth-profiles.json at {}: {}",
            auth_path.display(),
            e
        )
    })?;

    let profiles: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse OpenClaw auth-profiles.json: {}", e))?;

    let key = profiles
        .get("profiles")
        .and_then(|p| p.get("google:default"))
        .and_then(|v| v.get("key"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!("No google:default.key found in OpenClaw auth-profiles.json")
        })?;

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

    info!(
        nats_url = %cli.nats_url,
        model = %cli.model,
        max_tool_rounds = cli.max_tool_rounds,
        "Starting seidrum-llm-google provider plugin..."
    );

    // Resolve API key
    let api_key = resolve_google_api_key(&cli.google_api_key)?;

    // Connect to NATS
    let nats = async_nats::connect(&cli.nats_url).await?;
    info!("Connected to NATS");

    // Publish plugin registration
    let register = PluginRegister {
        id: "llm-google".to_string(),
        name: "LLM Google Provider".to_string(),
        version: "0.1.0".to_string(),
        description: "Google Gemini LLM provider — handles tool call loop internally".to_string(),
        consumes: vec!["llm.provider.google".to_string()],
        produces: vec!["llm.provider.google.response".to_string()],
        health_subject: "plugin.llm-google.health".to_string(),
    };
    let register_envelope = EventEnvelope::new(
        "plugin.register",
        "llm-google",
        None,
        None,
        &register,
    )?;
    nats.publish(
        "plugin.register",
        serde_json::to_vec(&register_envelope)?.into(),
    )
    .await?;
    info!("Published plugin.register");

    // Subscribe to llm.provider.google (request/reply service)
    let mut sub = nats.subscribe("llm.provider.google").await?;
    info!("Subscribed to llm.provider.google (request/reply)");

    // Build HTTP client
    let http = reqwest::Client::new();

    while let Some(msg) = sub.next().await {
        let reply = match &msg.reply {
            Some(r) => r.clone(),
            None => {
                warn!("Received llm.provider.google without reply subject, ignoring");
                continue;
            }
        };

        let nats_clone = nats.clone();
        let http_clone = http.clone();
        let api_key_clone = api_key.clone();
        let model = cli.model.clone();
        let max_tokens = cli.max_tokens;
        let max_tool_rounds = cli.max_tool_rounds;

        tokio::spawn(async move {
            let result = handle_provider_request(
                &msg.payload,
                &http_clone,
                &api_key_clone,
                &model,
                max_tokens,
                max_tool_rounds,
                &nats_clone,
            )
            .await;

            match result {
                Ok(response) => {
                    let resp_bytes = match serde_json::to_vec(&response) {
                        Ok(b) => b,
                        Err(e) => {
                            error!(error = %e, "Failed to serialize LlmResponse");
                            return;
                        }
                    };
                    if let Err(e) = nats_clone
                        .publish(reply.to_string(), resp_bytes.into())
                        .await
                    {
                        error!(error = %e, "Failed to reply to llm.provider.google request");
                    }
                }
                Err(e) => {
                    error!(error = %e, "Failed to handle llm.provider.google request");
                    // Send an error response back
                    let err_response = LlmResponse {
                        agent_id: "unknown".to_string(),
                        content: Some(format!("LLM provider error: {}", e)),
                        tool_calls: None,
                        model_used: model,
                        provider: "google".to_string(),
                        tokens: TokenUsage {
                            prompt_tokens: 0,
                            completion_tokens: 0,
                            total_tokens: 0,
                            estimated_cost_usd: 0.0,
                        },
                        duration_ms: 0,
                        finish_reason: "error".to_string(),
                    };
                    if let Ok(bytes) = serde_json::to_vec(&err_response) {
                        let _ = nats_clone
                            .publish(reply.to_string(), bytes.into())
                            .await;
                    }
                }
            }
        });
    }

    warn!("llm.provider.google subscription ended, shutting down");
    Ok(())
}

// ---------------------------------------------------------------------------
// Provider request handler
// ---------------------------------------------------------------------------

async fn handle_provider_request(
    payload: &[u8],
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    max_tokens: u32,
    max_tool_rounds: u32,
    nats: &async_nats::Client,
) -> Result<LlmResponse> {
    // Parse the UnifiedLlmRequest from the payload
    let request: UnifiedLlmRequest = serde_json::from_slice(payload)?;

    let agent_id = &request.agent_id;
    let correlation_id = request.correlation_id.as_deref();

    info!(
        agent_id = %agent_id,
        message_count = request.messages.len(),
        tool_count = request.tools.len(),
        "Handling llm.provider.google request"
    );

    // Convert unified messages to Gemini format
    let mut messages = unified_to_gemini_contents(&request.messages);

    // Convert unified tool schemas to Gemini tool declarations
    let tools_option = if request.tools.is_empty() {
        None
    } else {
        Some(unified_to_gemini_tools(&request.tools))
    };

    // Build system instruction from the request
    let system_instruction = request.system_prompt.as_ref().map(|s| {
        GeminiSystemInstruction {
            parts: vec![GeminiPart::text_part(s)],
        }
    });

    // Temperature from config
    let temperature = request.config.temperature.map(|t| t as f32);
    let response_max_tokens = request.config.max_tokens.unwrap_or(max_tokens);

    // Tool call loop
    let mut total_input_tokens: u32 = 0;
    let mut total_output_tokens: u32 = 0;
    let mut final_content: Option<String> = None;
    let start = Instant::now();

    for round in 0..=max_tool_rounds {
        info!(round, "LLM call round");

        let api_request = GeminiRequest {
            contents: messages.clone(),
            system_instruction: if round == 0 {
                system_instruction.clone()
            } else {
                system_instruction.clone()
            },
            generation_config: Some(GeminiGenerationConfig {
                temperature,
                max_output_tokens: Some(response_max_tokens),
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

        let candidate = api_response
            .candidates
            .first()
            .ok_or_else(|| anyhow::anyhow!("No candidates in Gemini response"))?;

        let finish_reason = candidate
            .finish_reason
            .clone()
            .unwrap_or_else(|| "unknown".to_string());

        info!(
            model = %model,
            input_tokens = total_input_tokens,
            output_tokens = total_output_tokens,
            finish_reason = %finish_reason,
            round,
            "Gemini API response received"
        );

        // Extract text content
        let content_text: String = candidate
            .content
            .parts
            .iter()
            .filter_map(|p| p.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n");

        // Extract function call parts
        let tool_calls = gemini_function_calls_to_tool_calls(&candidate.content.parts);

        if tool_calls.is_empty() || round == max_tool_rounds {
            // No tool calls or max rounds hit -- done
            if round == max_tool_rounds && !tool_calls.is_empty() {
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

        // Tool calls present -- execute them via tool dispatcher
        info!(
            tool_count = tool_calls.len(),
            round, "Executing tool calls via tool dispatcher"
        );

        // Append the model's response (with function_call parts) to messages
        messages.push(candidate.content.clone());

        // Dispatch each tool call via NATS request/reply to "tool.call"
        let mut response_parts: Vec<GeminiPart> = Vec::new();
        for tc in &tool_calls {
            let dispatch_req = tool_call_to_dispatch_request(tc, correlation_id);

            // Wrap in EventEnvelope for tool dispatcher
            let envelope = EventEnvelope::new(
                "tool.call",
                "llm-google",
                correlation_id.map(|s| s.to_string()),
                None,
                &dispatch_req,
            )?;
            let req_bytes = serde_json::to_vec(&envelope)?;

            let tool_result = match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                nats.request("tool.call", req_bytes.into()),
            )
            .await
            {
                Ok(Ok(resp_msg)) => {
                    match serde_json::from_slice::<ToolCallResponse>(&resp_msg.payload) {
                        Ok(resp) => {
                            info!(
                                tool_id = %resp.tool_id,
                                is_error = resp.is_error,
                                "Tool call completed"
                            );
                            resp
                        }
                        Err(e) => {
                            warn!(error = %e, "Failed to parse tool call response");
                            ToolCallResponse {
                                tool_id: tc.name.clone(),
                                result: serde_json::json!({
                                    "error": format!("Failed to parse response: {}", e)
                                }),
                                is_error: true,
                            }
                        }
                    }
                }
                Ok(Err(e)) => {
                    error!(error = %e, tool = %tc.name, "NATS request to tool.call failed");
                    ToolCallResponse {
                        tool_id: tc.name.clone(),
                        result: serde_json::json!({
                            "error": format!("Tool dispatch failed: {}", e)
                        }),
                        is_error: true,
                    }
                }
                Err(_) => {
                    warn!(tool = %tc.name, "Tool call timed out after 30s");
                    ToolCallResponse {
                        tool_id: tc.name.clone(),
                        result: serde_json::json!({
                            "error": "Tool call timed out"
                        }),
                        is_error: true,
                    }
                }
            };

            // Convert tool result to Gemini function response part
            let result_content = match serde_json::to_string(&tool_result.result) {
                Ok(s) => s,
                Err(_) => tool_result.result.to_string(),
            };
            response_parts.push(GeminiPart::function_response_part(
                &tc.name,
                serde_json::json!({
                    "result": result_content,
                    "is_error": tool_result.is_error,
                }),
            ));
        }

        // Append tool results as a user message with function_response parts
        messages.push(GeminiContent {
            role: "user".to_string(),
            parts: response_parts,
        });
    }

    let duration_ms = start.elapsed().as_millis() as u64;

    // Build the final LlmResponse
    let llm_response = LlmResponse {
        agent_id: agent_id.clone(),
        content: final_content,
        tool_calls: None, // Tool calls are resolved internally by this provider
        model_used: model.to_string(),
        provider: "google".to_string(),
        tokens: TokenUsage {
            prompt_tokens: total_input_tokens,
            completion_tokens: total_output_tokens,
            total_tokens: total_input_tokens + total_output_tokens,
            estimated_cost_usd: 0.0,
        },
        duration_ms,
        finish_reason: "stop".to_string(),
    };

    info!(
        agent_id = %agent_id,
        duration_ms,
        total_tokens = total_input_tokens + total_output_tokens,
        "LLM provider request completed"
    );

    Ok(llm_response)
}
