mod ollama_types;
mod translator;

use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use tracing::{error, info, warn};

use seidrum_common::events::{
    EventEnvelope, LlmResponse, PluginRegister, TokenUsage, ToolCallResponse, UnifiedLlmRequest,
};

use ollama_types::{OllamaMessage, OllamaRequest, OllamaResponse};
use translator::{
    ollama_tool_calls_to_unified, tool_call_to_dispatch_request, unified_to_ollama_messages,
    unified_to_ollama_tools,
};

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "seidrum-llm-ollama",
    about = "Seidrum Ollama LLM provider plugin"
)]
struct Cli {
    /// Bus server URL
    #[arg(long, env = "BUS_URL", default_value = "ws://127.0.0.1:9000")]
    bus_url: String,

    /// Ollama base URL
    #[arg(long, env = "OLLAMA_URL", default_value = "http://localhost:11434")]
    ollama_url: String,

    /// Model to use
    #[arg(long, env = "LLM_MODEL", default_value = "llama3.2")]
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
        bus_url = %cli.bus_url,
        ollama_url = %cli.ollama_url,
        model = %cli.model,
        max_tool_rounds = cli.max_tool_rounds,
        "Starting seidrum-llm-ollama provider plugin..."
    );

    // Connect to NATS
    let nats = seidrum_common::bus_client::BusClient::connect(&cli.bus_url, "llm-ollama").await?;
    info!("Connected to NATS");

    // Publish plugin registration
    let register = PluginRegister {
        id: "llm-ollama".to_string(),
        name: "LLM Ollama Provider".to_string(),
        version: "0.1.0".to_string(),
        description: "Ollama LLM provider — handles tool call loop internally".to_string(),
        consumes: vec!["llm.provider.ollama".to_string()],
        produces: vec!["llm.provider.ollama.response".to_string()],
        health_subject: "plugin.llm-ollama.health".to_string(),
        consumed_event_types: vec![],
        produced_event_types: vec![],
        config_schema: None,
    };
    let register_envelope =
        EventEnvelope::new("plugin.register", "llm-ollama", None, None, &register)?;
    nats.publish_bytes("plugin.register", serde_json::to_vec(&register_envelope)?)
        .await?;
    info!("Published plugin.register");

    // Subscribe to llm.provider.ollama (request/reply service)
    let mut sub = nats.subscribe("llm.provider.ollama").await?;
    info!("Subscribed to llm.provider.ollama (request/reply)");

    // Build HTTP client with timeouts
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(300)) // Ollama can be slower for large models
        .build()?;

    while let Some(msg) = sub.next().await {
        let reply = match &msg.reply {
            Some(r) => r.clone(),
            None => {
                warn!("Received llm.provider.ollama without reply subject, ignoring");
                continue;
            }
        };

        let nats_clone = nats.clone();
        let http_clone = http.clone();
        let ollama_url_clone = cli.ollama_url.clone();
        let model = cli.model.clone();
        let max_tokens = cli.max_tokens;
        let max_tool_rounds = cli.max_tool_rounds;

        tokio::spawn(async move {
            let result = handle_provider_request(
                &msg.payload,
                &http_clone,
                &ollama_url_clone,
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
                        error!(error = %e, "Failed to reply to llm.provider.ollama request");
                    }
                }
                Err(e) => {
                    error!(error = %e, "Failed to handle llm.provider.ollama request");
                    let err_response = LlmResponse {
                        agent_id: "unknown".to_string(),
                        content: Some(format!("LLM provider error: {}", e)),
                        tool_calls: None,
                        model_used: model,
                        provider: "ollama".to_string(),
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

    warn!("llm.provider.ollama subscription ended, shutting down");
    Ok(())
}

// ---------------------------------------------------------------------------
// Provider request handler
// ---------------------------------------------------------------------------

async fn handle_provider_request(
    payload: &[u8],
    http: &reqwest::Client,
    ollama_url: &str,
    model: &str,
    _max_tokens: u32,
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
        "Handling llm.provider.ollama request"
    );

    // Convert unified messages to Ollama format
    let mut messages = unified_to_ollama_messages(&request.messages);

    // Convert unified tool schemas to Ollama tool format
    let tools_option = if request.tools.is_empty() {
        None
    } else {
        Some(unified_to_ollama_tools(&request.tools))
    };

    // Temperature from config
    let temperature = request.config.temperature.map(|t| t as f32);

    // Tool call loop
    let mut total_input_tokens: u32 = 0;
    let mut total_output_tokens: u32 = 0;
    let mut final_content: Option<String> = None;
    let mut tool_rounds_count: u32 = 0;
    let start = Instant::now();

    for round in 0..=max_tool_rounds {
        info!(round, "LLM call round");

        let api_request = OllamaRequest {
            model: model.to_string(),
            messages: messages.clone(),
            temperature,
            tools: tools_option.clone(),
            stream: false,
        };

        let url = format!("{}/api/chat", ollama_url);

        let response = http
            .post(&url)
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
                    .and_then(|e| e.as_str())
                    .unwrap_or("Unknown error")
                    .to_string(),
                // Don't log raw body — it may contain sensitive information
                Err(_) => format!("Non-JSON error response (status {})", status),
            };
            anyhow::bail!("Ollama API error ({}): {}", status, err_msg);
        }

        let api_response: OllamaResponse = serde_json::from_slice(&body_bytes)?;

        // Update token counts
        if let Some(prompt_tokens) = api_response.prompt_eval_count {
            total_input_tokens += prompt_tokens;
        }
        if let Some(output_tokens) = api_response.eval_count {
            total_output_tokens += output_tokens;
        }

        info!(
            model = %model,
            input_tokens = total_input_tokens,
            output_tokens = total_output_tokens,
            round,
            "Ollama API response received"
        );

        // Extract text content
        let content_text = api_response.message.content.clone().unwrap_or_default();

        // Extract tool calls
        let tool_calls = api_response
            .message
            .tool_calls
            .as_ref()
            .map(|tcs| ollama_tool_calls_to_unified(tcs))
            .unwrap_or_default();

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
        tool_rounds_count += 1;
        info!(
            tool_count = tool_calls.len(),
            round, "Executing tool calls via tool dispatcher"
        );

        // Append the model's response to messages
        messages.push(api_response.message);

        // Dispatch each tool call via NATS request/reply to "capability.call"
        for tc in &tool_calls {
            let dispatch_req = tool_call_to_dispatch_request(tc, correlation_id);

            let envelope = EventEnvelope::new(
                "capability.call",
                "llm-ollama",
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

            // Convert tool result to Ollama tool result message
            let result_content = match serde_json::to_string(&tool_result.result) {
                Ok(s) => s,
                Err(_) => tool_result.result.to_string(),
            };
            messages.push(OllamaMessage::text("tool", &result_content));
        }
    }

    let duration_ms = start.elapsed().as_millis() as u64;

    // Build the final LlmResponse
    let llm_response = LlmResponse {
        agent_id: agent_id.clone(),
        content: final_content,
        tool_calls: None,
        model_used: model.to_string(),
        provider: "ollama".to_string(),
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
