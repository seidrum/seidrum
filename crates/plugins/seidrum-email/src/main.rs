use std::collections::HashMap;

use anyhow::Result;
use clap::Parser;
use futures::StreamExt;
use seidrum_common::events::{
    ChannelInbound, ChannelOutbound, EventEnvelope, PluginRegister, ToolCallRequest, ToolCallResponse,
};
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::{error, info, warn};
use anyhow::Context;

#[derive(Parser)]
#[command(name = "seidrum-email", about = "Seidrum Email channel plugin")]
struct Cli {
    /// NATS server URL
    #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4222")]
    nats_url: String,

    /// IMAP server hostname
    #[arg(long, env = "IMAP_HOST")]
    imap_host: String,

    /// IMAP server port
    #[arg(long, env = "IMAP_PORT", default_value = "993")]
    imap_port: u16,

    /// IMAP username
    #[arg(long, env = "IMAP_USER")]
    imap_user: String,

    /// IMAP password
    #[arg(long, env = "IMAP_PASSWORD")]
    imap_password: String,

    /// SMTP server hostname
    #[arg(long, env = "SMTP_HOST")]
    smtp_host: String,

    /// SMTP server port
    #[arg(long, env = "SMTP_PORT", default_value = "587")]
    smtp_port: u16,

    /// SMTP username
    #[arg(long, env = "SMTP_USER")]
    smtp_user: String,

    /// SMTP password
    #[arg(long, env = "SMTP_PASSWORD")]
    smtp_password: String,

    /// SMTP sender address
    #[arg(long, env = "SMTP_FROM")]
    smtp_from: String,

    /// Email polling interval in seconds
    #[arg(long, env = "EMAIL_POLL_INTERVAL", default_value = "60")]
    poll_interval: u64,
}

/// Poll IMAP for new unseen emails and publish them as inbound events.
async fn poll_imap(cli: &Cli, nats: &async_nats::Client) -> Result<()> {
    let addr = (cli.imap_host.as_str(), cli.imap_port);
    let tcp_stream = tokio::net::TcpStream::connect(addr).await?;
    // Wrap tokio TcpStream with compat layer for futures AsyncRead/AsyncWrite
    let compat_stream = tcp_stream.compat();
    let tls_connector = async_native_tls::TlsConnector::new();
    let stream = tls_connector
        .connect(&cli.imap_host, compat_stream)
        .await?;

    let client = async_imap::Client::new(stream);
    let mut session = client
        .login(&cli.imap_user, &cli.imap_password)
        .await
        .map_err(|e| anyhow::anyhow!("IMAP login failed: {}", e.0))?;

    session.select("INBOX").await?;

    // Search for unseen messages
    let search_result: Vec<u32> = session
        .search("UNSEEN")
        .await?
        .into_iter()
        .collect();

    if search_result.is_empty() {
        session.logout().await?;
        return Ok(());
    }

    info!(count = search_result.len(), "Found unseen emails");

    // Fetch the unseen messages
    let seq_set: String = search_result
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let messages_stream = session
        .fetch(&seq_set, "(RFC822 FLAGS)")
        .await?;

    // Collect messages so we can drop the stream before using session again
    let fetched: Vec<_> = messages_stream
        .collect::<Vec<_>>()
        .await;

    for fetch_result in fetched {
        let fetch = match fetch_result {
            Ok(f) => f,
            Err(err) => {
                error!(%err, "Failed to fetch email");
                continue;
            }
        };

        let body = match fetch.body() {
            Some(b) => b,
            None => {
                warn!("Email fetch had no body");
                continue;
            }
        };

        let parsed = match mail_parser::MessageParser::default().parse(body) {
            Some(msg) => msg,
            None => {
                warn!("Failed to parse email body");
                continue;
            }
        };

        let from = parsed
            .from()
            .and_then(|addrs| addrs.first())
            .and_then(|addr| addr.address())
            .unwrap_or("unknown")
            .to_string();

        let to = parsed
            .to()
            .and_then(|addrs| addrs.first())
            .and_then(|addr| addr.address())
            .unwrap_or("")
            .to_string();

        let subject = parsed
            .subject()
            .unwrap_or("")
            .to_string();

        let text_body = parsed
            .body_text(0)
            .map(|s| s.to_string())
            .unwrap_or_default();

        let message_id = parsed
            .message_id()
            .unwrap_or("")
            .to_string();

        let in_reply_to = parsed
            .in_reply_to()
            .as_text()
            .map(|s| s.to_string())
            .unwrap_or_default();

        // Thread ID: use In-Reply-To if present, otherwise Message-ID
        let chat_id = if !in_reply_to.is_empty() {
            in_reply_to.clone()
        } else {
            message_id.clone()
        };

        let combined_text = if subject.is_empty() {
            text_body.clone()
        } else {
            format!("{}\n\n{}", subject, text_body)
        };

        let mut metadata = HashMap::new();
        metadata.insert("from".to_string(), from.clone());
        metadata.insert("to".to_string(), to);
        metadata.insert("subject".to_string(), subject);
        metadata.insert("message_id".to_string(), message_id);
        if !in_reply_to.is_empty() {
            metadata.insert("in_reply_to".to_string(), in_reply_to);
        }

        let inbound = ChannelInbound {
            platform: "email".to_string(),
            user_id: from.clone(),
            chat_id,
            text: combined_text,
            reply_to: None,
            attachments: vec![],
            metadata,
        };

        let envelope = EventEnvelope::new(
            "channel.email.inbound",
            "email",
            None,
            None,
            &inbound,
        )?;

        let bytes = serde_json::to_vec(&envelope)?;
        nats.publish("channel.email.inbound", bytes.into()).await?;
        info!(from = %from, "Published channel.email.inbound event");
    }

    // Mark fetched messages as seen (consume the stream)
    let store_stream = session.store(&seq_set, "+FLAGS (\\Seen)").await?;
    let _: Vec<_> = store_stream.collect().await;
    session.logout().await?;

    Ok(())
}

/// Send an email via SMTP based on a ChannelOutbound event.
fn send_email(cli: &Cli, outbound: &ChannelOutbound) -> Result<()> {
    use lettre::message::header::ContentType;
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{Message, SmtpTransport, Transport};

    let recipient = &outbound.chat_id;

    let subject = outbound
        .actions
        .first()
        .map(|a| a.value.clone())
        .unwrap_or_else(|| "Seidrum".to_string());

    let email = Message::builder()
        .from(cli.smtp_from.parse()?)
        .to(recipient.parse()?)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(outbound.text.clone())?;

    let creds = Credentials::new(
        cli.smtp_user.clone(),
        cli.smtp_password.clone(),
    );

    let mailer = SmtpTransport::starttls_relay(&cli.smtp_host)?
        .port(cli.smtp_port)
        .credentials(creds)
        .build();

    mailer.send(&email)?;

    info!(to = %recipient, "Sent email via SMTP");
    Ok(())
}

/// Send an email via SMTP, used by the tool.call.email handler.
fn send_email_tool_impl(
    smtp_host: &str,
    smtp_port: u16,
    smtp_user: &str,
    smtp_password: &str,
    smtp_from: &str,
    to: &str,
    subject: &str,
    body: &str,
    in_reply_to: Option<&str>,
) -> Result<()> {
    use lettre::message::header::ContentType;
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{Message, SmtpTransport, Transport};

    let mut builder = Message::builder()
        .from(smtp_from.parse().context("Invalid from address")?)
        .to(to.parse().context("Invalid to address")?)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN);

    if let Some(reply_id) = in_reply_to {
        builder = builder.in_reply_to(reply_id.to_string());
    }

    let email = builder.body(body.to_string())?;

    let creds = Credentials::new(smtp_user.to_string(), smtp_password.to_string());

    let mailer = SmtpTransport::starttls_relay(smtp_host)?
        .port(smtp_port)
        .credentials(creds)
        .build();

    mailer.send(&email)?;

    info!(to = %to, subject = %subject, "Sent email via SMTP (tool call)");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    info!(
        nats_url = %cli.nats_url,
        imap_host = %cli.imap_host,
        smtp_host = %cli.smtp_host,
        poll_interval = cli.poll_interval,
        "Starting seidrum-email plugin..."
    );

    // Connect to NATS
    let nats = async_nats::connect(&cli.nats_url).await?;
    info!("Connected to NATS at {}", cli.nats_url);

    // Register plugin
    let register = PluginRegister {
        id: "email".to_string(),
        name: "Email Channel".to_string(),
        version: "0.1.0".to_string(),
        description: "Bidirectional email channel — IMAP ingest + SMTP sending".to_string(),
        consumes: vec!["channel.email.outbound".to_string()],
        produces: vec!["channel.email.inbound".to_string()],
        health_subject: "plugin.email.health".to_string(),
    };
    let register_envelope = EventEnvelope::new(
        "plugin.register",
        "email",
        None,
        None,
        &register,
    )?;
    let register_bytes = serde_json::to_vec(&register_envelope)?;
    nats.publish("plugin.register", register_bytes.into()).await?;
    info!("Published plugin.register event");

    // Register tool with the kernel's tool registry
    let send_email_tool = serde_json::json!({
        "tool_id": "send-email",
        "plugin_id": "email",
        "name": "Send Email",
        "summary_md": "Send an email via SMTP with subject, body, and optional in-reply-to header for threading.",
        "manual_md": "# Send Email\n\nSend an email via the configured SMTP server.\n\n## Parameters\n- `to` (string, required): Recipient email address\n- `subject` (string, required): Email subject line\n- `body` (string, required): Email body text\n- `in_reply_to` (string, optional): Message-ID to reply to for threading\n\n## Returns\nConfirmation that the email was sent.",
        "parameters": {
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "Recipient email address"
                },
                "subject": {
                    "type": "string",
                    "description": "Email subject line"
                },
                "body": {
                    "type": "string",
                    "description": "Email body text"
                },
                "in_reply_to": {
                    "type": "string",
                    "description": "Message-ID to reply to for threading"
                }
            },
            "required": ["to", "subject", "body"]
        },
        "call_subject": "tool.call.email"
    });

    nats.publish("tool.register", serde_json::to_vec(&send_email_tool)?.into())
        .await?;
    info!("Tool 'send-email' registered with kernel");

    // Subscribe to outbound emails
    let mut outbound_sub = nats.subscribe("channel.email.outbound").await?;
    info!("Subscribed to channel.email.outbound");

    // Clone for the IMAP polling task
    let nats_poll = nats.clone();
    let poll_interval = cli.poll_interval;
    let imap_host = cli.imap_host.clone();
    let imap_port = cli.imap_port;
    let imap_user = cli.imap_user.clone();
    let imap_password = cli.imap_password.clone();

    // Spawn IMAP polling task
    let imap_handle = tokio::spawn(async move {
        let poll_cli = Cli {
            nats_url: String::new(),
            imap_host,
            imap_port,
            imap_user,
            imap_password,
            smtp_host: String::new(),
            smtp_port: 0,
            smtp_user: String::new(),
            smtp_password: String::new(),
            smtp_from: String::new(),
            poll_interval,
        };

        loop {
            if let Err(err) = poll_imap(&poll_cli, &nats_poll).await {
                error!(%err, "IMAP poll error");
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(poll_interval)).await;
        }
    });

    // Spawn SMTP outbound handler
    let smtp_host = cli.smtp_host.clone();
    let smtp_port = cli.smtp_port;
    let smtp_user = cli.smtp_user.clone();
    let smtp_password = cli.smtp_password.clone();
    let smtp_from = cli.smtp_from.clone();

    let outbound_handle = tokio::spawn(async move {
        let send_cli = Cli {
            nats_url: String::new(),
            imap_host: String::new(),
            imap_port: 0,
            imap_user: String::new(),
            imap_password: String::new(),
            smtp_host,
            smtp_port,
            smtp_user,
            smtp_password,
            smtp_from,
            poll_interval: 0,
        };

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

            if let Err(err) = send_email(&send_cli, &outbound) {
                error!(%err, chat_id = %outbound.chat_id, "Failed to send email");
            }
        }
    });

    // Subscribe to tool.call.email for tool dispatch
    let mut tool_sub = nats.subscribe("tool.call.email").await?;
    info!("Subscribed to tool.call.email");

    let tool_nats = nats.clone();
    let tool_smtp_host = cli.smtp_host.clone();
    let tool_smtp_port = cli.smtp_port;
    let tool_smtp_user = cli.smtp_user.clone();
    let tool_smtp_password = cli.smtp_password.clone();
    let tool_smtp_from = cli.smtp_from.clone();

    let tool_handle = tokio::spawn(async move {
        while let Some(msg) = tool_sub.next().await {
            let reply = match msg.reply {
                Some(ref r) => r.clone(),
                None => {
                    warn!("Received tool.call.email without reply subject, skipping");
                    continue;
                }
            };

            let tool_request: ToolCallRequest = match serde_json::from_slice(&msg.payload) {
                Ok(r) => r,
                Err(err) => {
                    warn!(%err, "Failed to deserialize ToolCallRequest");
                    let error_response = ToolCallResponse {
                        tool_id: "send-email".to_string(),
                        result: serde_json::json!({"error": format!("Invalid request: {}", err)}),
                        is_error: true,
                    };
                    if let Err(e) = tool_nats
                        .publish(reply, serde_json::to_vec(&error_response).unwrap_or_default().into())
                        .await
                    {
                        error!(%e, "Failed to publish error reply");
                    }
                    continue;
                }
            };

            let tool_response = match tool_request.tool_id.as_str() {
                "send-email" => {
                    let to = tool_request.arguments.get("to")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let subject = tool_request.arguments.get("subject")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let body = tool_request.arguments.get("body")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let in_reply_to = tool_request.arguments.get("in_reply_to")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());

                    info!(to = %to, subject = %subject, "Sending email via tool");

                    match send_email_tool_impl(
                        &tool_smtp_host,
                        tool_smtp_port,
                        &tool_smtp_user,
                        &tool_smtp_password,
                        &tool_smtp_from,
                        &to,
                        &subject,
                        &body,
                        in_reply_to.as_deref(),
                    ) {
                        Ok(()) => ToolCallResponse {
                            tool_id: tool_request.tool_id,
                            result: serde_json::json!({"status": "sent", "to": to, "subject": subject}),
                            is_error: false,
                        },
                        Err(err) => ToolCallResponse {
                            tool_id: tool_request.tool_id,
                            result: serde_json::json!({"error": format!("{}", err)}),
                            is_error: true,
                        },
                    }
                }
                other => {
                    ToolCallResponse {
                        tool_id: other.to_string(),
                        result: serde_json::json!({"error": format!("Unknown tool_id: {}", other)}),
                        is_error: true,
                    }
                }
            };

            if let Err(err) = tool_nats
                .publish(reply, serde_json::to_vec(&tool_response).unwrap_or_default().into())
                .await
            {
                error!(%err, "Failed to publish tool call reply");
            }
        }
    });

    tokio::select! {
        result = imap_handle => {
            match result {
                Ok(()) => info!("IMAP poller stopped"),
                Err(err) => error!(%err, "IMAP poller panicked"),
            }
        }
        result = outbound_handle => {
            match result {
                Ok(()) => info!("Outbound handler stopped"),
                Err(err) => error!(%err, "Outbound handler panicked"),
            }
        }
        result = tool_handle => {
            match result {
                Ok(()) => info!("Tool call handler stopped"),
                Err(err) => error!(%err, "Tool call handler panicked"),
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use seidrum_common::events::ChannelAction;

    #[test]
    fn test_channel_inbound_roundtrip() {
        let mut metadata = HashMap::new();
        metadata.insert("from".to_string(), "alice@example.com".to_string());
        metadata.insert("to".to_string(), "bob@example.com".to_string());
        metadata.insert("subject".to_string(), "Hello".to_string());
        metadata.insert("message_id".to_string(), "<msg-1@example.com>".to_string());

        let inbound = ChannelInbound {
            platform: "email".to_string(),
            user_id: "alice@example.com".to_string(),
            chat_id: "<msg-1@example.com>".to_string(),
            text: "Hello\n\nThis is the body".to_string(),
            reply_to: None,
            attachments: vec![],
            metadata,
        };

        let json = serde_json::to_string(&inbound).unwrap();
        let deserialized: ChannelInbound = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.platform, "email");
        assert_eq!(deserialized.user_id, "alice@example.com");
        assert_eq!(deserialized.metadata.get("subject").unwrap(), "Hello");
    }

    #[test]
    fn test_outbound_envelope_roundtrip() {
        let outbound = ChannelOutbound {
            platform: "email".to_string(),
            chat_id: "bob@example.com".to_string(),
            text: "Reply body here".to_string(),
            format: "plain".to_string(),
            reply_to: None,
            actions: vec![ChannelAction {
                label: "subject".to_string(),
                action_type: "metadata".to_string(),
                value: "Re: Hello".to_string(),
            }],
        };

        let envelope = EventEnvelope::new(
            "channel.email.outbound",
            "response-formatter",
            None,
            None,
            &outbound,
        )
        .unwrap();

        let json = serde_json::to_string(&envelope).unwrap();
        let deserialized: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.event_type, "channel.email.outbound");

        let recovered: ChannelOutbound =
            serde_json::from_value(deserialized.payload).unwrap();
        assert_eq!(recovered.chat_id, "bob@example.com");
    }
}
