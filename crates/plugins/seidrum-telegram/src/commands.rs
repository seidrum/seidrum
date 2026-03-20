use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::StreamExt;
use seidrum_common::events::{
    EventEnvelope, ToolCallRequest, ToolCallResponse, ToolRegister, ToolSearchRequest,
    ToolSearchResponse,
};
use seidrum_common::nats_utils::NatsClient;
use teloxide::prelude::*;
use teloxide::types::{BotCommand, ThreadId};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// A single command entry in the registry.
#[derive(Debug, Clone)]
pub struct CommandEntry {
    pub name: String,
    pub description: String,
    pub usage: Option<String>,
    pub kind: CommandKind,
}

/// Whether a command is handled locally or dispatched via capability call.
#[derive(Debug, Clone)]
pub enum CommandKind {
    BuiltIn,
    Capability {
        capability_id: String,
        plugin_id: String,
        call_subject: String,
    },
}

/// Registry of all available commands (built-in + discovered).
#[derive(Clone)]
pub struct CommandRegistry {
    commands: Arc<RwLock<Vec<CommandEntry>>>,
}

impl CommandRegistry {
    fn new(commands: Vec<CommandEntry>) -> Self {
        Self {
            commands: Arc::new(RwLock::new(commands)),
        }
    }

    pub async fn list(&self) -> Vec<CommandEntry> {
        self.commands.read().await.clone()
    }

    async fn add(&self, entry: CommandEntry) {
        let mut cmds = self.commands.write().await;
        // Replace if a command with the same name already exists
        if let Some(pos) = cmds.iter().position(|c| c.name == entry.name) {
            cmds[pos] = entry;
        } else {
            cmds.push(entry);
        }
    }

    async fn find(&self, name: &str) -> Option<CommandEntry> {
        let cmds = self.commands.read().await;
        cmds.iter().find(|c| c.name == name).cloned()
    }
}

/// Built-in commands that the Telegram plugin handles directly.
fn builtin_commands() -> Vec<CommandEntry> {
    vec![
        CommandEntry {
            name: "start".to_string(),
            description: "Welcome to Seidrum".to_string(),
            usage: None,
            kind: CommandKind::BuiltIn,
        },
        CommandEntry {
            name: "help".to_string(),
            description: "List all available commands".to_string(),
            usage: None,
            kind: CommandKind::BuiltIn,
        },
        CommandEntry {
            name: "info".to_string(),
            description: "Show chat and thread info".to_string(),
            usage: None,
            kind: CommandKind::BuiltIn,
        },
    ]
}

/// Discover commands at boot time by querying the capability registry.
pub async fn discover_commands(nats: &NatsClient) -> CommandRegistry {
    let mut commands = builtin_commands();

    // Query for capabilities with kind "command" and "both"
    let discovered = query_capability_commands(nats).await;
    commands.extend(discovered);

    let registry = CommandRegistry::new(commands);
    info!(
        count = registry.commands.read().await.len(),
        "Command registry initialized"
    );
    registry
}

/// Query NATS capability.search for command-type capabilities.
async fn query_capability_commands(nats: &NatsClient) -> Vec<CommandEntry> {
    let mut entries = Vec::new();

    for kind_filter in &["command", "both"] {
        let req = ToolSearchRequest {
            query_text: String::new(),
            limit: Some(100),
            kind_filter: Some(kind_filter.to_string()),
        };

        match tokio::time::timeout(
            Duration::from_secs(3),
            nats.request::<_, ToolSearchResponse>("capability.search", &req),
        )
        .await
        {
            Ok(Ok(resp)) => {
                for tool in resp.tools {
                    let short_name = extract_command_name(&tool.tool_id, &tool.parameters);
                    // We need the full describe to get plugin_id and call_subject
                    let (plugin_id, call_subject) =
                        fetch_capability_details(nats, &tool.tool_id).await;

                    entries.push(CommandEntry {
                        name: short_name,
                        description: tool.summary_md.clone(),
                        usage: None,
                        kind: CommandKind::Capability {
                            capability_id: tool.tool_id,
                            plugin_id,
                            call_subject,
                        },
                    });
                }
            }
            Ok(Err(err)) => {
                warn!(%err, kind_filter, "Failed to search capabilities");
            }
            Err(_) => {
                warn!(kind_filter, "Capability search timed out (no registry running?)");
            }
        }
    }

    entries
}

/// Fetch plugin_id and call_subject for a capability via capability.describe.
async fn fetch_capability_details(nats: &NatsClient, tool_id: &str) -> (String, String) {
    use seidrum_common::events::{ToolDescribeRequest, ToolDescribeResponse};

    let req = ToolDescribeRequest {
        tool_id: tool_id.to_string(),
    };

    match tokio::time::timeout(
        Duration::from_secs(3),
        nats.request::<_, ToolDescribeResponse>("capability.describe", &req),
    )
    .await
    {
        Ok(Ok(resp)) => (resp.plugin_id, resp.call_subject),
        _ => {
            // Fallback: derive from tool_id
            let plugin_id = tool_id
                .split('.')
                .next()
                .unwrap_or("unknown")
                .to_string();
            let call_subject = format!("capability.call.{}", tool_id);
            (plugin_id, call_subject)
        }
    }
}

/// Extract a Telegram-compatible command name from a capability.
/// Uses metadata.command_alias if present in parameters JSON, otherwise
/// derives from the capability ID (replacing hyphens with underscores).
fn extract_command_name(tool_id: &str, parameters: &serde_json::Value) -> String {
    // Check for command_alias in the parameters metadata
    if let Some(alias) = parameters
        .get("metadata")
        .and_then(|m| m.get("command_alias"))
        .and_then(|v| v.as_str())
    {
        return alias.to_string();
    }

    // Fallback: use tool_id, replace hyphens with underscores
    tool_id.replace('-', "_")
}

/// Set the Telegram "/" menu commands via the Bot API.
pub async fn set_telegram_commands(bot: &Bot, registry: &CommandRegistry) {
    let commands = registry.list().await;
    let bot_commands: Vec<BotCommand> = commands
        .iter()
        .map(|c| BotCommand::new(&c.name, &c.description))
        .collect();

    match bot.set_my_commands(bot_commands).send().await {
        Ok(_) => info!("Telegram bot commands menu updated"),
        Err(err) => error!(%err, "Failed to set Telegram bot commands"),
    }
}

/// Execute a parsed command. Returns Ok(()) even for unknown commands
/// (sends an error message to the user instead of failing).
pub async fn execute_command(
    command: &str,
    args: &str,
    chat_id: i64,
    thread_id: Option<ThreadId>,
    user_id: u64,
    registry: &CommandRegistry,
    bot: &Bot,
    nats: &NatsClient,
) -> Result<()> {
    // Strip bot username suffix if present (e.g., "/help@my_bot" -> "help")
    let cmd_name = command
        .split('@')
        .next()
        .unwrap_or(command)
        .trim_start_matches('/');

    info!(command = %cmd_name, %chat_id, %user_id, "Executing command");

    let entry = registry.find(cmd_name).await;

    match entry {
        Some(entry) => match &entry.kind {
            CommandKind::BuiltIn => {
                execute_builtin(cmd_name, args, chat_id, thread_id, user_id, registry, bot)
                    .await
            }
            CommandKind::Capability {
                capability_id,
                plugin_id,
                call_subject,
            } => {
                execute_capability(
                    capability_id,
                    plugin_id,
                    call_subject,
                    args,
                    chat_id,
                    thread_id,
                    bot,
                    nats,
                )
                .await
            }
        },
        None => {
            send_reply(
                bot,
                chat_id,
                thread_id,
                "Unknown command. Type /help for available commands.",
            )
            .await
        }
    }
}

/// Handle built-in commands.
async fn execute_builtin(
    command: &str,
    _args: &str,
    chat_id: i64,
    thread_id: Option<ThreadId>,
    user_id: u64,
    registry: &CommandRegistry,
    bot: &Bot,
) -> Result<()> {
    match command {
        "start" => {
            send_reply(
                bot,
                chat_id,
                thread_id,
                "\u{1f44b} Welcome to Seidrum!\n\nI'm your personal AI assistant. \
                 Send me a message, voice note, or photo.\n\nType /help to see available commands.",
            )
            .await
        }
        "help" => {
            let commands = registry.list().await;
            let mut text = String::from("\u{2699}\u{fe0f} *Available Commands*\n\n");
            for cmd in &commands {
                text.push_str(&format!("/{}", cmd.name));
                if let Some(ref usage) = cmd.usage {
                    text.push_str(&format!(" {}", usage));
                }
                text.push_str(&format!(" — {}\n", cmd.description));
            }
            send_reply(bot, chat_id, thread_id, &text).await
        }
        "info" => {
            let thread_str = thread_id
                .map(|t| format!("{}", t.0))
                .unwrap_or_else(|| "none".to_string());
            let text = format!(
                "\u{2139}\u{fe0f} *Chat Info*\n\n\
                 Chat ID: `{}`\n\
                 Thread ID: `{}`\n\
                 User ID: `{}`",
                chat_id, thread_str, user_id
            );
            send_reply(bot, chat_id, thread_id, &text).await
        }
        _ => {
            send_reply(
                bot,
                chat_id,
                thread_id,
                "Unknown command. Type /help for available commands.",
            )
            .await
        }
    }
}

/// Execute a capability-backed command via NATS request/reply.
async fn execute_capability(
    capability_id: &str,
    plugin_id: &str,
    call_subject: &str,
    args: &str,
    chat_id: i64,
    thread_id: Option<ThreadId>,
    bot: &Bot,
    nats: &NatsClient,
) -> Result<()> {
    let arguments = serde_json::json!({
        "args": args,
        "chat_id": chat_id.to_string(),
    });

    let req = ToolCallRequest {
        tool_id: capability_id.to_string(),
        plugin_id: plugin_id.to_string(),
        arguments,
        correlation_id: Some(format!("telegram:{}:cmd", chat_id)),
    };

    match tokio::time::timeout(
        Duration::from_secs(30),
        nats.request::<_, ToolCallResponse>(call_subject, &req),
    )
    .await
    {
        Ok(Ok(resp)) => {
            let text = if resp.is_error {
                format!(
                    "\u{274c} Command failed: {}",
                    resp.result
                        .as_str()
                        .unwrap_or(&resp.result.to_string())
                )
            } else {
                resp.result
                    .as_str()
                    .unwrap_or(&resp.result.to_string())
                    .to_string()
            };
            send_reply(bot, chat_id, thread_id, &text).await
        }
        Ok(Err(err)) => {
            warn!(%err, capability_id, "Capability call failed");
            send_reply(
                bot,
                chat_id,
                thread_id,
                &format!("\u{274c} Command error: {}", err),
            )
            .await
        }
        Err(_) => {
            send_reply(
                bot,
                chat_id,
                thread_id,
                "\u{23f3} Command timed out. Please try again.",
            )
            .await
        }
    }
}

/// Send a text reply to the given chat, optionally in a thread.
async fn send_reply(bot: &Bot, chat_id: i64, thread_id: Option<ThreadId>, text: &str) -> Result<()> {
    let chat = ChatId(chat_id);
    let mut req = bot.send_message(chat, text);
    if let Some(tid) = thread_id {
        req = req.message_thread_id(tid);
    }
    req.send().await?;
    Ok(())
}

/// Spawn a background task that listens for `capability.registered` events
/// and refreshes the command registry + Telegram menu when new commands appear.
pub async fn spawn_capability_listener(
    nats: NatsClient,
    registry: CommandRegistry,
    bot: Bot,
) {
    tokio::spawn(async move {
        let mut sub = match nats.subscribe("capability.registered").await {
            Ok(sub) => sub,
            Err(err) => {
                error!(%err, "Failed to subscribe to capability.registered");
                return;
            }
        };

        info!("Listening for capability.registered events");

        while let Some(msg) = sub.next().await {
            let envelope: EventEnvelope = match serde_json::from_slice(&msg.payload) {
                Ok(e) => e,
                Err(err) => {
                    warn!(%err, "Failed to parse capability.registered envelope");
                    continue;
                }
            };

            let reg: ToolRegister = match serde_json::from_value(envelope.payload) {
                Ok(r) => r,
                Err(err) => {
                    warn!(%err, "Failed to parse ToolRegister from envelope payload");
                    continue;
                }
            };

            if reg.kind != "command" && reg.kind != "both" {
                continue;
            }

            info!(tool_id = %reg.tool_id, kind = %reg.kind, "New command capability registered");

            let short_name = extract_command_name(&reg.tool_id, &reg.parameters);

            let entry = CommandEntry {
                name: short_name,
                description: reg.summary_md,
                usage: None,
                kind: CommandKind::Capability {
                    capability_id: reg.tool_id,
                    plugin_id: reg.plugin_id,
                    call_subject: reg.call_subject,
                },
            };

            registry.add(entry).await;
            set_telegram_commands(&bot, &registry).await;
        }
    });
}
