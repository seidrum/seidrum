mod anthropic_types;
mod translator;

use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use tracing::{error, info, warn};

use seidrum_common::events::{
    EventEnvelope, LlmResponse, PluginRegister, TokenUsage, ToolCallResponse, UnifiedLlmRequest,
};

use anthropic_types::{AnthropicContent, AnthropicMessage, AnthropicRequest, AnthropicResponse};
use translator::{
    anthropic_content_to_text, anthropic_tool_uses_to_unified, tool_call_to_dispatch_request,
    unified_to_anthropic_messages, unified_to_anthropic_tools,
};

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "seidrum-llm-anthropic",
    about = "Seidrum Anthropic LLM provider plugin"
)]
struct Cli {
    /// NATS server URL
    #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4222")]
    nats_url: String,

    /// Anthropic API key
    #[arg(long, env = "ANTHROPIC_API_KEY")]
    anthropic_api_key: String,

    /// Model to use
    #[arg(long, env = "LLM_MODEL", default_value = "claude-3-5-sonnet-20241022")]
    model: String,

    /// Max tokens for response
    #[arg(long, env = "LLM_MAX_TOKENS", default_value = "4096")]
    max_tokens: u32,

    /// Maximum number of tool call rounds before forcing a final response
    #[arg(long, env = "LLM_MAX_TOOL_ROUNDS", default_value = "10")]
    max_tool_rounds: u32,
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
        "Starting seidrum-llm-anthropic provider plugin..."
    );

    // Connect to NATS
    let nats =
        seidrum_common::bus_client::BusClient::connect(&cli.nats_url, "llm-anthropic").await?;
    info!("Connected to NATS");

    // Publish plugin registration
    let register = PluginRegister {
        id: "llm-anthropic".to_string(),
        name: "LLM Anthropic Provider".to_string(),
        version: "0.1.0".to_string(),
        description: "Anthropic LLM provider — handles tool call loop internally".to_string(),
        consumes: vec!["llm.provider.anthropic".to_string()],
        produces: vec!["llm.provider.anthropic.response".to_string()],
        health_subject: "plugin.llm-anthropic.health".to_string(),
        consumed_event_types: vec![],
        produced_event_types: vec![],
        config_schema: None,
    };
    let register_envelope =
        EventEnvelope::new("plugin.register", "llm-anthropic", None, None, &register)?;
    nats.publish_bytes("plugin.register", serde_json::to_vec(&register_envelope)?)
        .await?;
    info!("Published plugin.register");

    // Subscribe to llm.provider.anthropic (request/reply service)
    let mut sub = nats.subscribe("llm.provider.anthropic").await?;
    info!("Subscribed to llm.provider.anthropic (request/reply)");

    // Build HTTP client with timeouts
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    while let Some(msg) = sub.next().await {
        let reply = match &msg.reply {
            Some(r) => r.clone(),
            None => {
                warn!("Received llm.provider.anthropic without reply subject, ignoring");
                continue;
            }
        };

        let nats_clone = nats.clone();
        let http_clone = http.clone();
        let api_key_clone = cli.anthropic_api_key.clone();
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
                        .publish_bytes(reply.to_string(), resp_bytes)
                        .await
                    {
                        error!(error = %e, "Failed to reply to llm.provider.anthropic request");
                    }
                }
                Err(e) => {
                    error!(error = %e, "Failed to handle llm.provider.anthropic request");
                    let err_response = LlmResponse {
                        agent_id: "unknown".to_string(),
                        content: Some(format!("LLM provider error: {}", e)),
                        tool_calls: None,
                        model_used: model,
                        provider: "anthropic".to_string(),
                        tokens: TokenUsage {
                            prompt_tokens: 0,
                            completion_tokens: 0,
                            total_tokens: 0,
                            estimated_cost_usd: 0.0,
                        },
                        duration_ms: 0,
                        finish_reason: "error".to_string(),
                        tool_rounds: 0,
                    };
                    if let Ok(bytes) = serde_json::to_vec(&err_response) {
                        let _ = nats_clone.publish_bytes(reply.to_string(), bytes).await;
                    }
                }
            }
        });
    }

    warn!("llm.provider.anthropic subscription ended, shutting down");
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
    nats: &seidrum_common::bus_client::BusClient,
) -> Result<LlmResponse> {
    let request: UnifiedLlmRequest = serde_json::from_slice(payload)?;

    let agent_id = &request.agent_id;
    let correlation_id = request.correlation_id.as_deref();

    info!(
        agent_id = %agent_id,
        message_count = request.messages.len(),
        tool_count = request.tools.len(),
        "Handling llm.provider.anthropic request"
    );

    // Convert unified messages to Anthropic format
    let mut messages = unified_to_anthropic_messages(&request.messages);

    // Convert unified tool schemas to Anthropic tool format
    let tools_option = if request.tools.is_empty() {
        None
    } else {
        Some(unified_to_anthropic_tools(&request.tools))
    };

    // Temperature from config
    let temperature = request.config.temperature.map(|t| t as f32);
    let response_max_tokens = request.config.max_tokens.unwrap_or(max_tokens);

    // Tool call loop
    let mut total_input_tokens: u32 = 0;
    let mut total_output_tokens: u32 = 0;
    let mut final_content: Option<String> = None;
    let mut tool_rounds_count: u32 = 0;
    let start = Instant::now();

    for round in 0..=max_tool_rounds {
        info!(round, "LLM call round");

        let api_request = AnthropicRequest {
            model: model.to_string(),
            messages: messages.clone(),
            system: request.system_prompt.clone(),
            temperature,
            max_tokens: response_max_tokens,
            tools: tools_option.clone(),
        };

        let response = http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .json(&api_request)
            .send()
            .await?;

        let status = response.status();
        let body_bytes = response.bytes().await?;

        if !status.is_success() {
            let err_msg = match serde_json::from_slice::<serde_json::Value>(&body_bytes) {
                Ok(val) => val
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown error")
                    .to_string(),
                // Don't log raw body — it may contain sensitive headers or tokens
                Err(_) => format!("Non-JSON error response (status {})", status),
            };
            anyhow::bail!("Anthropic API error ({}): {}", status, err_msg);
        }

        let api_response: AnthropicResponse = serde_json::from_slice(&body_bytes)?;

        total_input_tokens += api_response.usage.input_tokens;
        total_output_tokens += api_response.usage.output_tokens;

        let finish_reason = &api_response.stop_reason;

        info!(
            model = %model,
            input_tokens = total_input_tokens,
            output_tokens = total_output_tokens,
            finish_reason = %finish_reason,
            round,
            "Anthropic API response received"
        );

        // Extract text content
        let content_text = anthropic_content_to_text(&api_response.content);

        // Extract tool uses
        let tool_uses = anthropic_tool_uses_to_unified(&api_response.content);

        if tool_uses.is_empty() || round == max_tool_rounds {
            // No tool calls or max rounds hit -- done
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

        // Tool uses present -- execute them via tool dispatcher
        tool_rounds_count += 1;
        info!(
            tool_count = tool_uses.len(),
            round, "Executing tool uses via tool dispatcher"
        );

        // Append the model's response to messages
        messages.push(AnthropicMessage {
            role: "assistant".to_string(),
            content: api_response.content.clone(),
        });

        // Dispatch each tool use via NATS request/reply to "capability.call"
        let mut tool_results: Vec<AnthropicContent> = Vec::new();
        for tc in &tool_uses {
            let dispatch_req = tool_call_to_dispatch_request(tc, correlation_id);

            let envelope = EventEnvelope::new(
                "capability.call",
                "llm-anthropic",
                correlation_id.map(|s| s.to_string()),
                None,
                &dispatch_req,
            )?;
            let req_bytes = serde_json::to_vec(&envelope)?;

            let tool_result = match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                nats.request_bytes("capability.call", req_bytes),
            )
            .await
            {
                Ok(Ok(resp_msg)) => match serde_json::from_slice::<ToolCallResponse>(&resp_msg) {
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
                },
                Ok(Err(e)) => {
                    error!(error = %e, tool = %tc.name, "NATS request to capability.call failed");
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

            // Convert tool result to Anthropic tool result content
            let result_content = match serde_json::to_string(&tool_result.result) {
                Ok(s) => s,
                Err(_) => tool_result.result.to_string(),
            };
            tool_results.push(AnthropicContent::ToolResult {
                tool_use_id: tc.id.clone(),
                content: result_content,
                is_error: if tool_result.is_error {
                    Some(true)
                } else {
                    None
                },
            });
        }

        // Append tool results as a user message
        if !tool_results.is_empty() {
            messages.push(AnthropicMessage {
                role: "user".to_string(),
                content: tool_results,
            });
        }
    }

    let duration_ms = start.elapsed().as_millis() as u64;

    // Build the final LlmResponse
    let llm_response = LlmResponse {
        agent_id: agent_id.clone(),
        content: final_content,
        tool_calls: None,
        model_used: model.to_string(),
        provider: "anthropic".to_string(),
        tokens: TokenUsage {
            prompt_tokens: total_input_tokens,
            completion_tokens: total_output_tokens,
            total_tokens: total_input_tokens + total_output_tokens,
            estimated_cost_usd: 0.0,
        },
        duration_ms,
        finish_reason: "stop".to_string(),
        tool_rounds: tool_rounds_count,
    };

    info!(
        agent_id = %agent_id,
        duration_ms,
        total_tokens = total_input_tokens + total_output_tokens,
        "LLM provider request completed"
    );

    Ok(llm_response)
}
