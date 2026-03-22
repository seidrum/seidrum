use anyhow::Result;
use clap::Parser;
use futures::StreamExt as _;

use tracing::{error, info, warn};

use seidrum_common::events::{ChannelOutbound, EventEnvelope, LlmResponse, PluginRegister};
use seidrum_common::nats_utils::NatsClient;

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "seidrum-response-formatter",
    about = "Seidrum response formatter plugin"
)]
struct Cli {
    /// NATS server URL
    #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4222")]
    nats_url: String,
}

// ---------------------------------------------------------------------------
// Channel metadata embedded in correlation_id
// ---------------------------------------------------------------------------

/// Encodes the source channel info so the response-formatter can route back.
/// Convention: correlation_id may carry `{platform}:{chat_id}:{original_event_id}`
/// or we fall back to examining the envelope for hints.
#[derive(Debug, Clone)]
struct ChannelHint {
    platform: String,
    chat_id: String,
}

impl ChannelHint {
    /// Try to extract channel routing info from the correlation_id.
    ///
    /// Expected format: `{platform}:{chat_id}:{event_id}`
    /// Falls back to defaults if parsing fails.
    fn from_correlation_id(correlation_id: &Option<String>) -> Self {
        if let Some(cid) = correlation_id {
            let parts: Vec<&str> = cid.splitn(3, ':').collect();
            if parts.len() >= 2 {
                let platform = parts[0].to_string();
                let chat_id = parts[1].to_string();
                // Validate platform is a known value
                if matches!(platform.as_str(), "telegram" | "cli" | "web") {
                    return Self { platform, chat_id };
                }
            }
        }

        // Default to CLI if we cannot determine the source channel
        Self {
            platform: "cli".to_string(),
            chat_id: "default".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Channel hint extraction (origin-aware)
// ---------------------------------------------------------------------------

/// Extract channel routing info, preferring the origin field over correlation_id.
fn extract_channel_hint(envelope: &EventEnvelope) -> ChannelHint {
    // V2: use origin field if present (set by orchestrator)
    if let Some(ref origin) = envelope.origin {
        return ChannelHint {
            platform: origin.platform.clone(),
            chat_id: origin.chat_id.clone(),
        };
    }
    // Fallback: parse correlation_id (backward compat)
    ChannelHint::from_correlation_id(&envelope.correlation_id)
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Format the LLM response text for the target platform.
fn format_for_platform(text: &str, platform: &str) -> (String, String) {
    match platform {
        "telegram" => (format_telegram(text), "markdown".to_string()),
        "cli" => (format_cli(text), "plain".to_string()),
        _ => (text.to_string(), "plain".to_string()),
    }
}

/// Format text for Telegram (Telegram-flavoured Markdown).
///
/// Telegram uses MarkdownV2 which requires escaping certain characters.
/// For now we pass through the LLM markdown mostly as-is, since most LLM
/// output uses standard markdown that Telegram handles reasonably well.
fn format_telegram(text: &str) -> String {
    // Telegram MarkdownV2 requires escaping these characters outside of
    // code blocks: _ * [ ] ( ) ~ ` > # + - = | { } . !
    // However, the LLM likely already produces valid markdown, so we do
    // minimal transformation: just ensure the text is not empty.
    if text.is_empty() {
        return "(empty response)".to_string();
    }
    text.to_string()
}

/// Format text for CLI output (plain text, strip markdown artifacts).
fn format_cli(text: &str) -> String {
    if text.is_empty() {
        return "(empty response)".to_string();
    }
    // For CLI, strip common markdown formatting for cleaner terminal output
    text.replace("**", "").replace("__", "").replace("```", "")
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    info!(nats_url = %cli.nats_url, "Starting seidrum-response-formatter plugin...");

    // Connect to NATS
    let nats = NatsClient::connect(&cli.nats_url, "response-formatter").await?;

    // Publish plugin registration
    let register = PluginRegister {
        id: "response-formatter".to_string(),
        name: "Response Formatter".to_string(),
        version: "0.1.0".to_string(),
        description: "Formats LLM responses for target channel and publishes outbound events"
            .to_string(),
        consumes: vec!["llm.response".to_string()],
        produces: vec![
            "channel.telegram.outbound".to_string(),
            "channel.cli.outbound".to_string(),
        ],
        health_subject: "plugin.response-formatter.health".to_string(),
        consumed_event_types: vec![],
        produced_event_types: vec![],
    };
    nats.publish_envelope("plugin.register", None, None, &register)
        .await?;
    info!("Published plugin.register");

    // Subscribe to llm.response events
    let mut sub = nats.subscribe("llm.response").await?;
    info!("Subscribed to llm.response");

    // Process events
    while let Some(msg) = sub.next().await {
        if let Err(e) = handle_llm_response(&msg.payload, &nats).await {
            error!(error = %e, "Failed to process llm.response event");
        }
    }

    warn!("Subscription ended, shutting down");
    Ok(())
}

// ---------------------------------------------------------------------------
// Event handler
// ---------------------------------------------------------------------------

async fn handle_llm_response(payload: &[u8], nats: &NatsClient) -> Result<()> {
    // Parse the event envelope
    let envelope: EventEnvelope = serde_json::from_slice(payload)
        .map_err(|e| anyhow::anyhow!("Failed to parse EventEnvelope: {e}"))?;

    info!(
        event_id = %envelope.id,
        correlation_id = ?envelope.correlation_id,
        "Processing llm.response event"
    );

    // Parse the LLM response payload
    let llm_response: LlmResponse = serde_json::from_value(envelope.payload.clone())
        .map_err(|e| anyhow::anyhow!("Failed to parse LlmResponse payload: {e}"))?;

    // Extract the response text
    let response_text = match &llm_response.content {
        Some(text) if !text.is_empty() => text.clone(),
        _ => {
            // If there is no text content (e.g. only tool calls), skip formatting
            if llm_response.tool_calls.is_some() {
                info!("LLM response contains only tool calls, skipping outbound");
                return Ok(());
            }
            warn!("LLM response has no content, publishing empty response marker");
            "(no response)".to_string()
        }
    };

    // Determine target channel: prefer origin field, fall back to correlation_id
    let hint = extract_channel_hint(&envelope);

    info!(
        platform = %hint.platform,
        chat_id = %hint.chat_id,
        text_len = response_text.len(),
        model = %llm_response.model_used,
        "Formatting response for channel"
    );

    // Format the response for the target platform
    let (formatted_text, format_type) = format_for_platform(&response_text, &hint.platform);

    // Build the outbound event
    let outbound = ChannelOutbound {
        platform: hint.platform.clone(),
        chat_id: hint.chat_id,
        text: formatted_text,
        format: format_type,
        reply_to: None,
        actions: vec![],
    };

    // Publish to channel.{platform}.outbound
    let subject = format!("channel.{}.outbound", hint.platform);
    nats.publish_envelope(
        &subject,
        envelope.correlation_id.clone(),
        envelope.scope.clone(),
        &outbound,
    )
    .await?;

    info!(subject = %subject, "Published outbound event");

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_hint_telegram() {
        let cid = Some("telegram:12345:event-abc".to_string());
        let hint = ChannelHint::from_correlation_id(&cid);
        assert_eq!(hint.platform, "telegram");
        assert_eq!(hint.chat_id, "12345");
    }

    #[test]
    fn test_channel_hint_cli() {
        let cid = Some("cli:default:event-xyz".to_string());
        let hint = ChannelHint::from_correlation_id(&cid);
        assert_eq!(hint.platform, "cli");
        assert_eq!(hint.chat_id, "default");
    }

    #[test]
    fn test_channel_hint_unknown_defaults_to_cli() {
        let cid = Some("some-random-ulid".to_string());
        let hint = ChannelHint::from_correlation_id(&cid);
        assert_eq!(hint.platform, "cli");
        assert_eq!(hint.chat_id, "default");
    }

    #[test]
    fn test_channel_hint_none_defaults_to_cli() {
        let hint = ChannelHint::from_correlation_id(&None);
        assert_eq!(hint.platform, "cli");
        assert_eq!(hint.chat_id, "default");
    }

    #[test]
    fn test_format_cli_strips_markdown() {
        let input = "**bold** and __underline__ and ```code```";
        let output = format_cli(input);
        assert_eq!(output, "bold and underline and code");
    }

    #[test]
    fn test_format_cli_empty() {
        assert_eq!(format_cli(""), "(empty response)");
    }

    #[test]
    fn test_format_telegram_passthrough() {
        let input = "**bold** text with [link](http://example.com)";
        let output = format_telegram(input);
        assert_eq!(output, input);
    }

    #[test]
    fn test_format_telegram_empty() {
        assert_eq!(format_telegram(""), "(empty response)");
    }

    #[test]
    fn test_format_for_platform_returns_correct_format_type() {
        let (_, fmt) = format_for_platform("hello", "telegram");
        assert_eq!(fmt, "markdown");

        let (_, fmt) = format_for_platform("hello", "cli");
        assert_eq!(fmt, "plain");

        let (_, fmt) = format_for_platform("hello", "unknown");
        assert_eq!(fmt, "plain");
    }
}
