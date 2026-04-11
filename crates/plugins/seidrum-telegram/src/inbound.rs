use std::collections::HashMap;

use anyhow::Result;
use seidrum_common::bus_client::BusClient;
use seidrum_common::events::{Attachment, ChannelInbound};
use teloxide::prelude::*;
use teloxide::types::Message;
use tracing::{info, warn};

use crate::commands::CommandRegistry;

/// Configuration for processing inbound Telegram messages.
pub struct InboundConfig {
    pub token: String,
    pub whisper_cli: String,
    pub whisper_model: String,
}

/// Main entry point for handling an incoming Telegram message.
/// Dispatches based on content type (text, voice, photo, document)
/// and publishes the appropriate event to NATS.
pub async fn handle_message(
    msg: &Message,
    nats: &BusClient,
    config: &InboundConfig,
    is_edited: bool,
    bot: &Bot,
    registry: &CommandRegistry,
) -> Result<()> {
    let user_id = msg
        .from
        .as_ref()
        .map(|u| u.id.0.to_string())
        .unwrap_or_else(|| "0".to_string());

    let chat_id = msg.chat.id.0.to_string();

    let thread_id = msg.thread_id.map(|id| id.to_string());
    let message_id = msg.id.0.to_string();

    // Build metadata with thread_id and message_id
    let mut metadata = HashMap::new();
    if let Some(ref tid) = thread_id {
        metadata.insert("thread_id".to_string(), tid.clone());
    }
    metadata.insert("message_id".to_string(), message_id);

    // Determine content type and build the event
    if let Some(text) = msg.text() {
        // Text messages: check for commands first
        if text.starts_with('/') {
            let parts: Vec<&str> = text.splitn(2, char::is_whitespace).collect();
            let cmd_name = parts[0].trim_start_matches('/');
            let args = parts.get(1).copied().unwrap_or("");
            let uid = msg.from.as_ref().map(|u| u.id.0).unwrap_or(0);
            return crate::commands::execute_command(
                cmd_name,
                args,
                msg.chat.id.0,
                msg.thread_id,
                uid,
                registry,
                bot,
                nats,
            )
            .await;
        }

        let display_text = if is_edited {
            format!("[The user edited their previous message to:]\n{}", text)
        } else {
            text.to_string()
        };

        let inbound = build_inbound(
            &user_id,
            &chat_id,
            &display_text,
            msg.reply_to_message().map(|r| r.id.0.to_string()),
            vec![],
            metadata,
        );

        publish_inbound(nats, &inbound).await?;
    } else if let Some(voice) = msg.voice() {
        let transcript = match crate::voice::transcribe_voice(
            &config.token,
            &voice.file.id,
            &config.whisper_cli,
            &config.whisper_model,
        )
        .await
        {
            Ok(Some(transcript)) => {
                format!(
                    "[Voice message transcription \u{2014} some words may be mistranscribed]\n{}",
                    transcript
                )
            }
            Ok(None) => "[Voice message received but transcript was empty]".to_string(),
            Err(err) => {
                warn!(%err, "Voice transcription failed");
                "[Voice message received but transcription failed]".to_string()
            }
        };

        let display_text = if is_edited {
            format!(
                "[The user edited their previous message to:]\n{}",
                transcript
            )
        } else {
            transcript
        };

        let inbound = build_inbound(
            &user_id,
            &chat_id,
            &display_text,
            msg.reply_to_message().map(|r| r.id.0.to_string()),
            vec![],
            metadata,
        );

        publish_inbound(nats, &inbound).await?;
    } else if let Some(photos) = msg.photo() {
        // Pick the largest photo (last in the Vec<PhotoSize>)
        if let Some(photo) = photos.last() {
            let attachment = crate::media::download_photo(
                &config.token,
                &photo.file.id,
                Some(photo.file.size as u64),
            )
            .await?;

            let caption = msg.caption().unwrap_or("The user sent an image.");
            let display_text = if is_edited {
                format!("[The user edited their previous message to:]\n{}", caption)
            } else {
                caption.to_string()
            };

            let inbound = build_inbound(
                &user_id,
                &chat_id,
                &display_text,
                msg.reply_to_message().map(|r| r.id.0.to_string()),
                vec![attachment],
                metadata,
            );

            publish_inbound(nats, &inbound).await?;
        }
    } else if let Some(doc) = msg.document() {
        let mime = doc
            .mime_type
            .as_ref()
            .map(|m| m.to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());

        let file_name = doc.file_name.as_deref().unwrap_or("unknown");

        let attachment = crate::media::download_document(
            &config.token,
            &doc.file.id,
            Some(mime),
            Some(doc.file.size as u64),
        )
        .await?;

        let caption = msg
            .caption()
            .map(|c| c.to_string())
            .unwrap_or_else(|| format!("Document: {}", file_name));

        let display_text = if is_edited {
            format!("[The user edited their previous message to:]\n{}", caption)
        } else {
            caption
        };

        let inbound = build_inbound(
            &user_id,
            &chat_id,
            &display_text,
            msg.reply_to_message().map(|r| r.id.0.to_string()),
            vec![attachment],
            metadata,
        );

        publish_inbound(nats, &inbound).await?;
    } else {
        // Unsupported message type — log and skip
        info!(
            chat_id = %chat_id,
            user_id = %user_id,
            "Received unsupported message type, skipping"
        );
    }

    Ok(())
}

/// Helper to build a ChannelInbound event.
fn build_inbound(
    user_id: &str,
    chat_id: &str,
    text: &str,
    reply_to: Option<String>,
    attachments: Vec<Attachment>,
    metadata: HashMap<String, String>,
) -> ChannelInbound {
    ChannelInbound {
        platform: "telegram".to_string(),
        user_id: user_id.to_string(),
        chat_id: chat_id.to_string(),
        text: text.to_string(),
        reply_to,
        attachments,
        metadata,
    }
}

/// Wrap a ChannelInbound in an EventEnvelope and publish to NATS.
async fn publish_inbound(nats: &BusClient, inbound: &ChannelInbound) -> Result<()> {
    // Build correlation_id in the format "telegram:{chat_id}:{message_id}"
    // The response formatter uses this to route outbound messages back to the right channel
    let msg_id = inbound
        .metadata
        .get("message_id")
        .cloned()
        .unwrap_or_default();
    let correlation_id = format!("telegram:{}:{}", inbound.chat_id, msg_id);

    nats.publish_envelope(
        "channel.telegram.inbound",
        Some(correlation_id),
        None,
        inbound,
    )
    .await?;

    info!(
        user_id = %inbound.user_id,
        chat_id = %inbound.chat_id,
        "Published channel.telegram.inbound"
    );

    Ok(())
}
