mod commands;
mod inbound;
mod markdown;
mod media;
mod outbound;
mod split;
mod voice;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use seidrum_common::bus_client::BusClient;
use seidrum_common::events::PluginRegister;
use teloxide::prelude::*;
use teloxide::types::Message;
use tracing::{error, info, warn};

use crate::inbound::InboundConfig;

#[derive(Parser)]
#[command(name = "seidrum-telegram", about = "Seidrum Telegram channel plugin")]
struct Cli {
    /// NATS server URL
    #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4222")]
    nats_url: String,

    /// Telegram Bot API token
    #[arg(long, env = "TELEGRAM_TOKEN")]
    telegram_token: String,

    /// Comma-separated list of allowed Telegram user IDs (empty = allow all)
    #[arg(long, env = "TELEGRAM_ALLOWED_USERS", default_value = "")]
    allowed_users: String,

    /// Path to whisper-cli binary for voice transcription
    #[arg(long, env = "WHISPER_CLI_PATH", default_value = "whisper-cli")]
    whisper_cli_path: String,

    /// Path to whisper model file
    #[arg(long, env = "WHISPER_MODEL_PATH", default_value = "ggml-small.en.bin")]
    whisper_model_path: String,
}

/// Parse comma-separated user IDs into a vec for fast lookup.
fn parse_allowed_users(raw: &str) -> Vec<u64> {
    raw.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<u64>().ok())
        .collect()
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let allowed_users = parse_allowed_users(&cli.allowed_users);

    info!(
        nats_url = %cli.nats_url,
        allowed_users_count = allowed_users.len(),
        "Starting seidrum-telegram plugin..."
    );

    // Connect to NATS
    let nats = BusClient::connect(&cli.nats_url, "telegram").await?;
    info!("Connected to NATS at {}", cli.nats_url);

    // Register plugin
    let register = PluginRegister {
        id: "telegram".to_string(),
        name: "Telegram Channel".to_string(),
        version: "0.1.0".to_string(),
        description: "Bridges Telegram Bot API to NATS events".to_string(),
        consumes: vec!["channel.telegram.outbound".to_string()],
        produces: vec![
            "channel.telegram.inbound".to_string(),
            "command.execute".to_string(),
        ],
        health_subject: "plugin.telegram.health".to_string(),
        consumed_event_types: vec![],
        produced_event_types: vec![],
        config_schema: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "allowed_users": {
                    "type": "string",
                    "description": "Comma-separated list of allowed Telegram user IDs (empty = allow all)",
                    "default": ""
                },
                "whisper_cli_path": {
                    "type": "string",
                    "description": "Path to whisper-cli binary for voice transcription",
                    "default": "whisper-cli"
                },
                "whisper_model_path": {
                    "type": "string",
                    "description": "Path to whisper model file",
                    "default": "ggml-small.en.bin"
                }
            }
        })),
    };
    nats.publish_envelope("plugin.register", None, None, &register)
        .await?;
    info!("Published plugin.register event");

    // Create the Telegram bot
    let bot = Bot::new(&cli.telegram_token);

    // Spawn outbound loop: NATS -> Telegram
    let outbound_nats = nats.clone();
    let outbound_bot = bot.clone();
    let outbound_handle =
        tokio::spawn(async move { outbound::run_outbound_loop(outbound_bot, outbound_nats).await });

    // Discover commands and set the Telegram "/" menu
    let registry = commands::discover_commands(&nats).await;
    commands::set_telegram_commands(&bot, &registry).await;

    // Spawn live-refresh listener for new capability registrations
    commands::spawn_capability_listener(nats.clone(), registry.clone(), bot.clone()).await;

    // Prepare shared state for the inbound handler
    let allowed = Arc::new(allowed_users);
    let nats_arc = Arc::new(nats);
    let registry_arc = Arc::new(registry);
    let config = Arc::new(InboundConfig {
        token: cli.telegram_token.clone(),
        whisper_cli: cli.whisper_cli_path,
        whisper_model: cli.whisper_model_path,
    });

    // Inbound handler: Telegram -> NATS
    // Handles both new messages and edited messages
    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint({
            let config = config.clone();
            let nats_arc = nats_arc.clone();
            let allowed = allowed.clone();
            let registry_arc = registry_arc.clone();
            move |bot: Bot, msg: Message| {
                let config = config.clone();
                let nats_arc = nats_arc.clone();
                let allowed = allowed.clone();
                let registry_arc = registry_arc.clone();
                async move {
                    if let Err(err) = handle_inbound(
                        &msg,
                        &nats_arc,
                        &config,
                        &allowed,
                        false,
                        &bot,
                        &registry_arc,
                    )
                    .await
                    {
                        error!(%err, "Failed to handle inbound message");
                    }
                    respond(())
                }
            }
        }))
        .branch(Update::filter_edited_message().endpoint({
            let config = config.clone();
            let nats_arc = nats_arc.clone();
            let allowed = allowed.clone();
            let registry_arc = registry_arc.clone();
            move |bot: Bot, msg: Message| {
                let config = config.clone();
                let nats_arc = nats_arc.clone();
                let allowed = allowed.clone();
                let registry_arc = registry_arc.clone();
                async move {
                    if let Err(err) = handle_inbound(
                        &msg,
                        &nats_arc,
                        &config,
                        &allowed,
                        true,
                        &bot,
                        &registry_arc,
                    )
                    .await
                    {
                        error!(%err, "Failed to handle edited inbound message");
                    }
                    respond(())
                }
            }
        }));

    info!("Starting Telegram bot polling...");
    let mut dispatcher = Dispatcher::builder(bot, handler)
        .default_handler(|upd| async move {
            tracing::debug!("Unhandled update: {:?}", upd);
        })
        .build();

    tokio::select! {
        _ = dispatcher.dispatch() => {
            info!("Telegram dispatcher stopped");
        }
        result = outbound_handle => {
            match result {
                Ok(()) => info!("Outbound handler stopped"),
                Err(err) => error!(%err, "Outbound handler panicked"),
            }
        }
    }

    Ok(())
}

/// Shared inbound handler logic: whitelist check + dispatch to inbound module.
async fn handle_inbound(
    msg: &Message,
    nats: &BusClient,
    config: &InboundConfig,
    allowed: &[u64],
    is_edited: bool,
    bot: &Bot,
    registry: &commands::CommandRegistry,
) -> anyhow::Result<()> {
    let user_id = msg.from.as_ref().map(|u| u.id.0).unwrap_or(0);

    // Check user whitelist
    if !allowed.is_empty() && !allowed.contains(&user_id) {
        warn!(user_id, "Rejecting message from non-whitelisted user");
        return Ok(());
    }

    inbound::handle_message(msg, nats, config, is_edited, bot, registry).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_allowed_users_empty() {
        let result = parse_allowed_users("");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_allowed_users_single() {
        let result = parse_allowed_users("12345");
        assert_eq!(result, vec![12345]);
    }

    #[test]
    fn test_parse_allowed_users_multiple() {
        let result = parse_allowed_users("111,222,333");
        assert_eq!(result, vec![111, 222, 333]);
    }

    #[test]
    fn test_parse_allowed_users_with_spaces() {
        let result = parse_allowed_users("111, 222 , 333");
        assert_eq!(result, vec![111, 222, 333]);
    }

    #[test]
    fn test_parse_allowed_users_invalid_skipped() {
        let result = parse_allowed_users("111,abc,333");
        assert_eq!(result, vec![111, 333]);
    }
}
