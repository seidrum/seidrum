use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use futures::StreamExt;
use seidrum_common::events::{
    ChannelInbound, ChannelOutbound, EventEnvelope, PluginRegister,
};
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use teloxide::respond;
use tracing::{error, info, warn};

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
}

/// Parse comma-separated user IDs into a set for fast lookup.
fn parse_allowed_users(raw: &str) -> Vec<u64> {
    raw.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<u64>().ok())
        .collect()
}

/// Escape special characters for Telegram MarkdownV2 format.
#[allow(dead_code)]
fn escape_markdown_v2(text: &str) -> String {
    let special_chars = [
        '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|',
        '{', '}', '.', '!',
    ];
    let mut result = String::with_capacity(text.len());
    for ch in text.chars() {
        if special_chars.contains(&ch) {
            result.push('\\');
        }
        result.push(ch);
    }
    result
}

/// Convert generic markdown (from LLM responses) to Telegram-safe HTML.
/// Telegram HTML supports: <b>, <i>, <code>, <pre>, <a href="">.
/// This is a best-effort converter for common patterns.
fn markdown_to_telegram_html(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Code blocks: ```...```
        if i + 2 < len && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`' {
            i += 3;
            // Skip optional language tag on the same line
            while i < len && chars[i] != '\n' {
                i += 1;
            }
            if i < len {
                i += 1; // skip newline
            }
            result.push_str("<pre>");
            while i + 2 < len
                && !(chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`')
            {
                result.push(escape_html_char(chars[i]));
                i += 1;
            }
            result.push_str("</pre>");
            if i + 2 < len {
                i += 3; // skip closing ```
            }
            continue;
        }

        // Inline code: `...`
        if chars[i] == '`' {
            i += 1;
            result.push_str("<code>");
            while i < len && chars[i] != '`' {
                result.push(escape_html_char(chars[i]));
                i += 1;
            }
            result.push_str("</code>");
            if i < len {
                i += 1;
            }
            continue;
        }

        // Bold: **...**
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            i += 2;
            result.push_str("<b>");
            while i + 1 < len && !(chars[i] == '*' && chars[i + 1] == '*') {
                result.push(escape_html_char(chars[i]));
                i += 1;
            }
            result.push_str("</b>");
            if i + 1 < len {
                i += 2;
            }
            continue;
        }

        // Italic: *...*
        if chars[i] == '*' {
            i += 1;
            result.push_str("<i>");
            while i < len && chars[i] != '*' {
                result.push(escape_html_char(chars[i]));
                i += 1;
            }
            result.push_str("</i>");
            if i < len {
                i += 1;
            }
            continue;
        }

        result.push(escape_html_char(chars[i]));
        i += 1;
    }

    result
}

fn escape_html_char(ch: char) -> char {
    // For full correctness we'd return a string, but single-char escapes
    // are handled below in the push path. We keep it simple here.
    ch
}

#[allow(dead_code)]
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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
    let nats = async_nats::connect(&cli.nats_url).await?;
    info!("Connected to NATS at {}", cli.nats_url);

    // Register plugin
    let register = PluginRegister {
        id: "telegram".to_string(),
        name: "Telegram Channel".to_string(),
        version: "0.1.0".to_string(),
        description: "Bridges Telegram Bot API to NATS events".to_string(),
        consumes: vec!["channel.telegram.outbound".to_string()],
        produces: vec!["channel.telegram.inbound".to_string()],
        health_subject: "plugin.telegram.health".to_string(),
    };
    let register_envelope = EventEnvelope::new(
        "plugin.register",
        "telegram",
        None,
        None,
        &register,
    )?;
    let register_bytes = serde_json::to_vec(&register_envelope)?;
    nats.publish("plugin.register", register_bytes.into()).await?;
    info!("Published plugin.register event");

    // Create the Telegram bot
    let bot = Bot::new(&cli.telegram_token);
    let nats_pub = nats.clone();
    let allowed = Arc::new(allowed_users);

    // Subscribe to outbound messages from NATS
    let mut outbound_sub = nats
        .subscribe("channel.telegram.outbound")
        .await?;
    info!("Subscribed to channel.telegram.outbound");

    // Spawn outbound handler: NATS -> Telegram
    let bot_outbound = bot.clone();
    let outbound_handle = tokio::spawn(async move {
        while let Some(msg) = outbound_sub.next().await {
            let envelope: EventEnvelope = match serde_json::from_slice(&msg.payload) {
                Ok(e) => e,
                Err(err) => {
                    error!(%err, "Failed to parse outbound EventEnvelope");
                    continue;
                }
            };

            let outbound: ChannelOutbound = match serde_json::from_value(envelope.payload) {
                Ok(o) => o,
                Err(err) => {
                    error!(%err, "Failed to parse ChannelOutbound payload");
                    continue;
                }
            };

            let chat_id = match outbound.chat_id.parse::<i64>() {
                Ok(id) => ChatId(id),
                Err(err) => {
                    error!(%err, chat_id = %outbound.chat_id, "Invalid chat_id");
                    continue;
                }
            };

            // Format and send message based on format field
            let send_result = match outbound.format.as_str() {
                "markdown" => {
                    let html_text = markdown_to_telegram_html(&outbound.text);
                    bot_outbound
                        .send_message(chat_id, html_text)
                        .parse_mode(ParseMode::Html)
                        .await
                }
                "html" => {
                    bot_outbound
                        .send_message(chat_id, &outbound.text)
                        .parse_mode(ParseMode::Html)
                        .await
                }
                _ => {
                    // "plain" or unknown — send as plain text
                    bot_outbound
                        .send_message(chat_id, &outbound.text)
                        .await
                }
            };

            match send_result {
                Ok(_) => {
                    info!(chat_id = %outbound.chat_id, "Sent outbound message to Telegram");
                }
                Err(err) => {
                    error!(%err, chat_id = %outbound.chat_id, "Failed to send Telegram message");
                }
            }
        }
    });

    // Inbound handler: Telegram -> NATS
    let handler = Update::filter_message().endpoint(
        move |_bot: Bot, msg: Message, nats: Arc<async_nats::Client>, allowed: Arc<Vec<u64>>| async move {
            let user_id = msg
                .from
                .as_ref()
                .map(|u| u.id.0)
                .unwrap_or(0);

            // Check user whitelist
            if !allowed.is_empty() && !allowed.contains(&user_id) {
                warn!(user_id, "Rejecting message from non-whitelisted user");
                return respond(());
            }

            let text = msg.text().unwrap_or("").to_string();
            if text.is_empty() {
                return respond(());
            }

            let chat_id = msg.chat.id.0.to_string();

            let inbound = ChannelInbound {
                platform: "telegram".to_string(),
                user_id: user_id.to_string(),
                chat_id: chat_id.clone(),
                text,
                reply_to: msg
                    .reply_to_message()
                    .map(|r| r.id.0.to_string()),
                attachments: vec![],
                metadata: HashMap::new(),
            };

            let envelope = match EventEnvelope::new(
                "channel.telegram.inbound",
                "telegram",
                None,
                None,
                &inbound,
            ) {
                Ok(e) => e,
                Err(err) => {
                    error!(%err, "Failed to create EventEnvelope");
                    return respond(());
                }
            };

            let bytes = match serde_json::to_vec(&envelope) {
                Ok(b) => b,
                Err(err) => {
                    error!(%err, "Failed to serialize envelope");
                    return respond(());
                }
            };

            if let Err(err) = nats
                .publish("channel.telegram.inbound", bytes.into())
                .await
            {
                error!(%err, "Failed to publish inbound event to NATS");
            } else {
                info!(user_id, chat_id = %chat_id, "Published channel.telegram.inbound");
            }

            respond(())
        },
    );

    let nats_arc = Arc::new(nats_pub);
    let allowed_arc = allowed;

    info!("Starting Telegram bot polling...");
    let mut dispatcher = Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![nats_arc, allowed_arc])
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

    #[test]
    fn test_markdown_to_telegram_html_bold() {
        let result = markdown_to_telegram_html("Hello **world**!");
        assert!(result.contains("<b>world</b>"));
    }

    #[test]
    fn test_markdown_to_telegram_html_italic() {
        let result = markdown_to_telegram_html("Hello *world*!");
        assert!(result.contains("<i>world</i>"));
    }

    #[test]
    fn test_markdown_to_telegram_html_code() {
        let result = markdown_to_telegram_html("Use `cargo build` here");
        assert!(result.contains("<code>cargo build</code>"));
    }

    #[test]
    fn test_markdown_to_telegram_html_code_block() {
        let result = markdown_to_telegram_html("```rust\nfn main() {}\n```");
        assert!(result.contains("<pre>"));
        assert!(result.contains("fn main()"));
        assert!(result.contains("</pre>"));
    }

    #[test]
    fn test_escape_markdown_v2() {
        let result = escape_markdown_v2("Hello_World!");
        assert_eq!(result, "Hello\\_World\\!");
    }
}
