use anyhow::Result;
use seidrum_common::bus_client::BusClient;
use seidrum_common::events::{ChannelOutbound, EventEnvelope};
use teloxide::prelude::*;
use teloxide::types::{ChatId, MessageId, ParseMode, ReplyParameters, ThreadId};
use tracing::{error, info, warn};

/// Run the outbound loop: subscribe to NATS for outbound messages and send them
/// to Telegram. Handles message splitting, markdown formatting with fallback,
/// and reply-to threading.
pub async fn run_outbound_loop(bot: Bot, nats: BusClient) {
    let mut subscriber = match nats.subscribe("channel.telegram.outbound").await {
        Ok(s) => s,
        Err(err) => {
            error!(%err, "Failed to subscribe to channel.telegram.outbound");
            return;
        }
    };

    info!("Outbound loop started, listening on channel.telegram.outbound");

    while let Some(msg) = subscriber.next().await {
        if let Err(err) = handle_outbound_message(&bot, &msg.payload).await {
            error!(%err, "Failed to handle outbound message");
        }
    }

    info!("Outbound loop ended");
}

/// Process a single outbound NATS message and send it to Telegram.
async fn handle_outbound_message(bot: &Bot, payload: &[u8]) -> Result<()> {
    let envelope: EventEnvelope = serde_json::from_slice(payload)?;
    let outbound: ChannelOutbound = serde_json::from_value(envelope.payload)?;

    let chat_id = ChatId(outbound.chat_id.parse::<i64>()?);

    // Extract optional thread_id from the outbound metadata or actions
    // (ChannelOutbound doesn't have a metadata field, so we check if thread_id
    // was encoded in the reply_to or actions)
    let thread_id: Option<ThreadId> = None; // Future: extract from metadata if added

    // Split long messages
    let chunks = crate::split::split_message(&outbound.text, 4000);

    for (i, chunk) in chunks.iter().enumerate() {
        // Only set reply_to on the first chunk
        let reply_to = if i == 0 {
            outbound
                .reply_to
                .as_ref()
                .and_then(|id| id.parse::<i32>().ok())
                .map(MessageId)
        } else {
            None
        };

        let sent =
            send_with_fallback(bot, chat_id, chunk, &outbound.format, reply_to, thread_id).await;

        if let Err(err) = sent {
            error!(
                %err,
                chat_id = %outbound.chat_id,
                chunk_index = i,
                "Failed to send message chunk to Telegram"
            );
        }
    }

    info!(
        chat_id = %outbound.chat_id,
        chunks = chunks.len(),
        "Sent outbound message to Telegram"
    );

    Ok(())
}

/// Send a message to Telegram, trying MarkdownV2 first (for markdown format),
/// falling back to plain text if Telegram rejects the formatting.
async fn send_with_fallback(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    format: &str,
    reply_to: Option<MessageId>,
    thread_id: Option<ThreadId>,
) -> Result<()> {
    match format {
        "markdown" => {
            let escaped = crate::markdown::escape_markdown_v2(text);
            let mut request = bot.send_message(chat_id, &escaped);
            request = request.parse_mode(ParseMode::MarkdownV2);

            if let Some(reply_id) = reply_to {
                request = request.reply_parameters(ReplyParameters::new(reply_id));
            }
            if let Some(tid) = thread_id {
                request = request.message_thread_id(tid);
            }

            match request.await {
                Ok(_) => return Ok(()),
                Err(err) => {
                    warn!(
                        %err,
                        "MarkdownV2 send failed, retrying as plain text"
                    );
                    // Fall through to plain text
                }
            }

            // Fallback: strip markdown and send as plain text
            let plain = crate::markdown::strip_markdown(text);
            let mut request = bot.send_message(chat_id, &plain);
            if let Some(reply_id) = reply_to {
                request = request.reply_parameters(ReplyParameters::new(reply_id));
            }
            if let Some(tid) = thread_id {
                request = request.message_thread_id(tid);
            }
            request.await?;
        }
        "html" => {
            let mut request = bot.send_message(chat_id, text);
            request = request.parse_mode(ParseMode::Html);
            if let Some(reply_id) = reply_to {
                request = request.reply_parameters(ReplyParameters::new(reply_id));
            }
            if let Some(tid) = thread_id {
                request = request.message_thread_id(tid);
            }
            request.await?;
        }
        _ => {
            // "plain" or unknown
            let mut request = bot.send_message(chat_id, text);
            if let Some(reply_id) = reply_to {
                request = request.reply_parameters(ReplyParameters::new(reply_id));
            }
            if let Some(tid) = thread_id {
                request = request.message_thread_id(tid);
            }
            request.await?;
        }
    }

    Ok(())
}
