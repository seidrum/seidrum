use std::collections::HashMap;

use anyhow::Result;
use seidrum_common::events::{Attachment, ChannelInbound, EventEnvelope};
use seidrum_common::nats_utils::NatsClient;
use serde::{Deserialize, Serialize};
use teloxide::types::Message;
use tracing::{info, warn};

/// Configuration for processing inbound Telegram messages.
pub struct InboundConfig {
    pub token: String,
    pub whisper_cli: String,
    pub whisper_model: String,
}

/// Command parsed from a "/" prefixed message.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct CommandExecute {
    pub command: String,
    pub args: Vec<String>,
    pub channel: String,
    pub chat_id: String,
    pub thread_id: Option<String>,
    pub user_id: String,
}

/// Main entry point for handling an incoming Telegram message.
/// Dispatches based on content type (text, voice, photo, document)
/// and publishes the appropriate event to NATS.
pub async fn handle_message(
    msg: &Message,
    nats: &NatsClient,
    config: &InboundConfig,
    is_edited: bool,
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
            return handle_command(text, &user_id, &chat_id, &thread_id, nats).await;
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
            Ok(None) => {
                "[Voice message received but transcript was empty]".to_string()
            }
            Err(err) => {
                warn!(%err, "Voice transcription failed");
                "[Voice message received but transcription failed]".to_string()
            }
        };

        let display_text = if is_edited {
            format!("[The user edited their previous message to:]\n{}", transcript)
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

        let file_name = doc
            .file_name
            .as_deref()
            .unwrap_or("unknown");

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

/// Parse a "/" command and publish to "command.execute".
async fn handle_command(
    text: &str,
    user_id: &str,
    chat_id: &str,
    thread_id: &Option<String>,
    nats: &NatsClient,
) -> Result<()> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let command_name = parts
        .first()
        .map(|s| s.trim_start_matches('/'))
        .unwrap_or("");

    if command_name.is_empty() {
        return Ok(());
    }

    let args: Vec<String> = parts
        .iter()
        .skip(1)
        .map(|s| s.to_string())
        .collect();

    let cmd = CommandExecute {
        command: command_name.to_string(),
        args,
        channel: "telegram".to_string(),
        chat_id: chat_id.to_string(),
        thread_id: thread_id.clone(),
        user_id: user_id.to_string(),
    };

    let envelope = EventEnvelope::new(
        "command.execute",
        "telegram",
        None,
        None,
        &cmd,
    )?;

    nats.publish("command.execute", &envelope).await?;
    info!(
        command = %command_name,
        user_id = %user_id,
        chat_id = %chat_id,
        "Published command.execute"
    );

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
async fn publish_inbound(nats: &NatsClient, inbound: &ChannelInbound) -> Result<()> {
    nats.publish_envelope(
        "channel.telegram.inbound",
        None,
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
