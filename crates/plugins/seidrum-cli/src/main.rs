use std::collections::HashMap;

use anyhow::Result;
use clap::Parser;
use seidrum_common::bus_client::BusClient;
use seidrum_common::events::{ChannelInbound, ChannelOutbound, EventEnvelope, PluginRegister};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{error, info};

#[derive(Parser)]
#[command(name = "seidrum-cli", about = "Seidrum CLI channel plugin")]
struct Cli {
    /// Bus server URL
    #[arg(long, env = "BUS_URL", default_value = "ws://127.0.0.1:9000")]
    bus_url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    info!(bus_url = %cli.bus_url, "Starting seidrum-cli plugin...");

    // Connect to NATS
    let nats = BusClient::connect(&cli.bus_url, "cli").await?;

    // Register with kernel
    let registration = PluginRegister {
        id: "cli".into(),
        name: "CLI Channel".into(),
        version: "0.1.0".into(),
        description: "Interactive CLI channel for development and testing".into(),
        consumes: vec!["channel.cli.outbound".into()],
        produces: vec!["channel.cli.inbound".into()],
        health_subject: "plugin.cli.health".into(),
        consumed_event_types: vec![],
        produced_event_types: vec![],
        config_schema: None,
    };
    nats.publish_envelope("plugin.register", None, None, &registration)
        .await?;
    info!("Registered cli plugin with kernel");

    // Subscribe to outbound messages
    let mut outbound_sub = nats.subscribe("channel.cli.outbound").await?;

    // Task 1: read stdin, publish channel.cli.inbound events
    let nats_stdin = nats.clone();
    let stdin_task = tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let reader = BufReader::new(stdin);
        let mut lines = reader.lines();
        let mut stdout = tokio::io::stdout();

        // Print initial prompt
        if let Err(e) = stdout.write_all(b"> ").await {
            error!("Failed to write prompt: {e}");
            return;
        }
        let _ = stdout.flush().await;

        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim().to_string();
            if trimmed.is_empty() {
                // Re-print prompt for empty lines
                let _ = stdout.write_all(b"> ").await;
                let _ = stdout.flush().await;
                continue;
            }

            let inbound = ChannelInbound {
                platform: "cli".into(),
                user_id: "cli-user".into(),
                chat_id: "cli-session".into(),
                text: trimmed,
                reply_to: None,
                attachments: vec![],
                metadata: HashMap::new(),
            };

            if let Err(e) = nats_stdin
                .publish_envelope("channel.cli.inbound", None, None, &inbound)
                .await
            {
                error!("Failed to publish inbound event: {e}");
            }

            // Print prompt for next input
            let _ = stdout.write_all(b"> ").await;
            let _ = stdout.flush().await;
        }

        info!("stdin closed, shutting down input task");
    });

    // Task 2: subscribe to channel.cli.outbound, print to stdout
    let outbound_task = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();

        while let Some(msg) = outbound_sub.next().await {
            match serde_json::from_slice::<EventEnvelope>(&msg.payload) {
                Ok(envelope) => {
                    match serde_json::from_value::<ChannelOutbound>(envelope.payload) {
                        Ok(outbound) => {
                            // Print a newline before the response to separate from prompt
                            let formatted = format!("\n{}\n\n> ", outbound.text);
                            let _ = stdout.write_all(formatted.as_bytes()).await;
                            let _ = stdout.flush().await;
                        }
                        Err(e) => {
                            error!("Failed to deserialize ChannelOutbound payload: {e}");
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to deserialize EventEnvelope: {e}");
                }
            }
        }

        info!("Outbound subscription closed");
    });

    // Wait for either task to finish (stdin closing will end the program)
    tokio::select! {
        _ = stdin_task => {
            info!("Input task finished, exiting");
        }
        _ = outbound_task => {
            info!("Outbound task finished, exiting");
        }
    }

    Ok(())
}
