use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::StreamExt;
use seidrum_common::bus_client::BusClient;
use seidrum_common::events::{
    PluginRegister, StorageDeleteRequest, StorageDeleteResponse, StorageGetRequest,
    StorageGetResponse, StorageSetRequest, StorageSetResponse, ToolCallRequest, ToolCallResponse,
    ToolSearchRequest, ToolSearchResponse,
};
use serde::{Deserialize, Serialize};
use teloxide::prelude::*;
use teloxide::types::{BotCommand, ThreadId};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// Mirror of kernel's RegistryQuery for request/reply to registry.query.
#[derive(Serialize)]
#[serde(tag = "query_type")]
enum RegistryQuery {
    #[serde(rename = "list_plugins")]
    ListPlugins,
}

/// Mirror of kernel's RegistryQueryResponse.
#[derive(Deserialize)]
struct RegistryQueryResponse {
    #[allow(dead_code)]
    success: bool,
    plugins: Option<Vec<PluginRegister>>,
}

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

    /// Remove a command by its capability_id. Returns true if removed.
    async fn remove_by_capability_id(&self, capability_id: &str) -> bool {
        let mut cmds = self.commands.write().await;
        let before = cmds.len();
        cmds.retain(|c| {
            if let CommandKind::Capability {
                capability_id: ref cid,
                ..
            } = c.kind
            {
                cid != capability_id
            } else {
                true
            }
        });
        cmds.len() < before
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
        CommandEntry {
            name: "plugins".to_string(),
            description: "List active plugins".to_string(),
            usage: None,
            kind: CommandKind::BuiltIn,
        },
        CommandEntry {
            name: "link".to_string(),
            description: "Link this thread to a project directory".to_string(),
            usage: Some("<path>".to_string()),
            kind: CommandKind::BuiltIn,
        },
        CommandEntry {
            name: "unlink".to_string(),
            description: "Unlink this thread from its project directory".to_string(),
            usage: None,
            kind: CommandKind::BuiltIn,
        },
        CommandEntry {
            name: "restart".to_string(),
            description: "Restart the Telegram plugin".to_string(),
            usage: None,
            kind: CommandKind::BuiltIn,
        },
    ]
}

/// Discover commands at boot time by querying the capability registry.
pub async fn discover_commands(nats: &BusClient) -> CommandRegistry {
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
async fn query_capability_commands(nats: &BusClient) -> Vec<CommandEntry> {
    let mut entries = Vec::new();

    let mut seen_ids = std::collections::HashSet::new();

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
                    if !seen_ids.insert(tool.tool_id.clone()) {
                        continue;
                    }
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
                warn!(
                    kind_filter,
                    "Capability search timed out (no registry running?)"
                );
            }
        }
    }

    entries
}

/// Fetch plugin_id and call_subject for a capability via capability.describe.
async fn fetch_capability_details(nats: &BusClient, tool_id: &str) -> (String, String) {
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
            let plugin_id = tool_id.split('.').next().unwrap_or("unknown").to_string();
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
    nats: &BusClient,
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
                execute_builtin(
                    cmd_name, args, chat_id, thread_id, user_id, registry, bot, nats,
                )
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
    args: &str,
    chat_id: i64,
    thread_id: Option<ThreadId>,
    user_id: u64,
    registry: &CommandRegistry,
    bot: &Bot,
    nats: &BusClient,
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
            let mut text = String::from("\u{2699}\u{fe0f} <b>Available Commands</b>\n\n");
            for cmd in &commands {
                text.push_str(&format!("/{}", cmd.name));
                if let Some(ref usage) = cmd.usage {
                    text.push_str(&format!(" {}", usage));
                }
                text.push_str(&format!(" — {}\n", cmd.description));
            }
            send_html_reply(bot, chat_id, thread_id, &text).await
        }
        "info" => {
            let thread_str = thread_id
                .map(|t| format!("{}", t.0))
                .unwrap_or_else(|| "none".to_string());
            let text = format!(
                "\u{2139}\u{fe0f} <b>Chat Info</b>\n\n\
                 Chat ID: <code>{}</code>\n\
                 Thread ID: <code>{}</code>\n\
                 User ID: <code>{}</code>",
                chat_id, thread_str, user_id
            );
            send_html_reply(bot, chat_id, thread_id, &text).await
        }
        "plugins" => {
            let text = match tokio::time::timeout(
                Duration::from_secs(3),
                nats.request::<_, RegistryQueryResponse>(
                    "registry.query",
                    &RegistryQuery::ListPlugins,
                ),
            )
            .await
            {
                Ok(Ok(resp)) => {
                    let plugins = resp.plugins.unwrap_or_default();
                    if plugins.is_empty() {
                        "No plugins registered.".to_string()
                    } else {
                        let mut text = format!("<b>Active Plugins</b> ({})\n\n", plugins.len());
                        for p in &plugins {
                            text.push_str(&format!(
                                "<b>{}</b> v{}\n{}\n\n",
                                p.name, p.version, p.description
                            ));
                        }
                        text
                    }
                }
                Ok(Err(err)) => format!("Failed to query plugins: {}", err),
                Err(_) => "Plugin registry query timed out.".to_string(),
            };
            send_html_reply(bot, chat_id, thread_id, &text).await
        }
        "link" => {
            let tid = match thread_id {
                Some(tid) => tid,
                None => {
                    return send_reply(
                        bot,
                        chat_id,
                        thread_id,
                        "/link can only be used inside a thread.",
                    )
                    .await;
                }
            };

            let path = args.trim();
            if path.is_empty() {
                return send_reply(
                    bot,
                    chat_id,
                    thread_id,
                    "Usage: /link <path>\nExample: /link /home/user/projects/myapp",
                )
                .await;
            }

            let storage_key = format!("{}:{}", chat_id, tid.0);
            let req = StorageSetRequest {
                plugin_id: "telegram".to_string(),
                namespace: "thread_links".to_string(),
                key: storage_key,
                value: serde_json::json!({ "working_dir": path }),
            };

            match tokio::time::timeout(
                Duration::from_secs(5),
                nats.request::<_, StorageSetResponse>("storage.set", &req),
            )
            .await
            {
                Ok(Ok(resp)) if resp.success => {
                    info!(%chat_id, thread_id = %tid.0, %path, "Thread linked to project");
                    send_html_reply(
                        bot,
                        chat_id,
                        thread_id,
                        &format!("Thread linked to <code>{}</code>", path),
                    )
                    .await
                }
                Ok(Ok(resp)) => {
                    let err_msg = resp.error.unwrap_or_else(|| "unknown error".to_string());
                    send_reply(
                        bot,
                        chat_id,
                        thread_id,
                        &format!("Failed to save link: {}", err_msg),
                    )
                    .await
                }
                Ok(Err(err)) => {
                    warn!(%err, "storage.set request failed");
                    send_reply(bot, chat_id, thread_id, "Failed to save link.").await
                }
                Err(_) => send_reply(bot, chat_id, thread_id, "Storage request timed out.").await,
            }
        }
        "unlink" => {
            let tid = match thread_id {
                Some(tid) => tid,
                None => {
                    return send_reply(
                        bot,
                        chat_id,
                        thread_id,
                        "/unlink can only be used inside a thread.",
                    )
                    .await;
                }
            };

            let storage_key = format!("{}:{}", chat_id, tid.0);
            let req = StorageDeleteRequest {
                plugin_id: "telegram".to_string(),
                namespace: "thread_links".to_string(),
                key: storage_key,
            };

            match tokio::time::timeout(
                Duration::from_secs(5),
                nats.request::<_, StorageDeleteResponse>("storage.delete", &req),
            )
            .await
            {
                Ok(Ok(resp)) if resp.existed => {
                    info!(%chat_id, thread_id = %tid.0, "Thread unlinked");
                    send_reply(bot, chat_id, thread_id, "Thread unlinked from project.").await
                }
                Ok(Ok(_)) => {
                    send_reply(
                        bot,
                        chat_id,
                        thread_id,
                        "This thread is not linked to any project.",
                    )
                    .await
                }
                Ok(Err(err)) => {
                    warn!(%err, "storage.delete request failed");
                    send_reply(bot, chat_id, thread_id, "Failed to unlink.").await
                }
                Err(_) => send_reply(bot, chat_id, thread_id, "Storage request timed out.").await,
            }
        }
        "restart" => {
            send_reply(bot, chat_id, thread_id, "Restarting Telegram plugin...").await?;
            info!("Restart requested via /restart command");
            std::process::exit(0);
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

/// Look up the linked project directory for a thread via plugin storage.
async fn get_thread_working_dir(
    nats: &BusClient,
    chat_id: i64,
    thread_id: Option<ThreadId>,
) -> Option<String> {
    let tid = thread_id?;
    let storage_key = format!("{}:{}", chat_id, tid.0);
    let req = StorageGetRequest {
        plugin_id: "telegram".to_string(),
        namespace: "thread_links".to_string(),
        key: storage_key,
    };

    match tokio::time::timeout(
        Duration::from_secs(3),
        nats.request::<_, StorageGetResponse>("storage.get", &req),
    )
    .await
    {
        Ok(Ok(resp)) if resp.found => resp.value.and_then(|v| {
            v.get("working_dir")
                .and_then(|d| d.as_str().map(|s| s.to_string()))
        }),
        _ => None,
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
    nats: &BusClient,
) -> Result<()> {
    let mut arguments = serde_json::json!({
        "args": args,
        "chat_id": chat_id.to_string(),
    });

    // Inject linked working_dir if this thread is linked to a project
    if let Some(working_dir) = get_thread_working_dir(nats, chat_id, thread_id).await {
        arguments
            .as_object_mut()
            .unwrap()
            .insert("working_dir".to_string(), serde_json::json!(working_dir));
    }

    let req = ToolCallRequest {
        tool_id: capability_id.to_string(),
        plugin_id: plugin_id.to_string(),
        arguments,
        correlation_id: Some(format!("telegram:{}:cmd", chat_id)),
    };

    match tokio::time::timeout(
        Duration::from_secs(120),
        nats.request::<_, ToolCallResponse>(call_subject, &req),
    )
    .await
    {
        Ok(Ok(resp)) => {
            let text = if resp.is_error {
                let msg = resp
                    .result
                    .as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| resp.result.to_string());
                format!("\u{274c} Command failed: {}", msg)
            } else {
                // Handle both string results and JSON objects with a "result" field
                resp.result
                    .as_str()
                    .map(|s| s.to_string())
                    .or_else(|| {
                        resp.result
                            .get("result")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_else(|| resp.result.to_string())
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

/// Send a plain text reply to the given chat, optionally in a thread.
async fn send_reply(
    bot: &Bot,
    chat_id: i64,
    thread_id: Option<ThreadId>,
    text: &str,
) -> Result<()> {
    let chat = ChatId(chat_id);
    let mut req = bot.send_message(chat, text);
    if let Some(tid) = thread_id {
        req = req.message_thread_id(tid);
    }
    req.send().await?;
    Ok(())
}

/// Send an HTML-formatted reply to the given chat, optionally in a thread.
/// Falls back to plain text if Telegram rejects the HTML.
async fn send_html_reply(
    bot: &Bot,
    chat_id: i64,
    thread_id: Option<ThreadId>,
    text: &str,
) -> Result<()> {
    let chat = ChatId(chat_id);
    let mut req = bot.send_message(chat, text);
    req = req.parse_mode(teloxide::types::ParseMode::Html);
    if let Some(tid) = thread_id {
        req = req.message_thread_id(tid);
    }

    match req.send().await {
        Ok(_) => Ok(()),
        Err(err) => {
            warn!(%err, "HTML send failed, retrying as plain text");
            let mut req = bot.send_message(chat, &crate::markdown::strip_markdown(text));
            if let Some(tid) = thread_id {
                req = req.message_thread_id(tid);
            }
            req.send().await?;
            Ok(())
        }
    }
}

/// Capability entry published by the kernel on `capability.registered`.
#[derive(Deserialize)]
struct CapabilityRegistered {
    tool_id: String,
    plugin_id: String,
    #[allow(dead_code)]
    name: String,
    summary_md: String,
    parameters: serde_json::Value,
    call_subject: String,
    #[serde(default)]
    kind: String,
}

/// Event published by the kernel on `capability.deregistered`.
#[derive(Deserialize)]
struct CapabilityDeregistered {
    tool_id: String,
    #[allow(dead_code)]
    plugin_id: String,
}

/// Spawn a background task that listens for `capability.registered` and
/// `capability.deregistered` events to keep the command registry in sync.
pub async fn spawn_capability_listener(nats: BusClient, registry: CommandRegistry, bot: Bot) {
    tokio::spawn(async move {
        let mut reg_sub = match nats.subscribe("capability.registered").await {
            Ok(sub) => sub,
            Err(err) => {
                error!(%err, "Failed to subscribe to capability.registered");
                return;
            }
        };

        let mut dereg_sub = match nats.subscribe("capability.deregistered").await {
            Ok(sub) => sub,
            Err(err) => {
                error!(%err, "Failed to subscribe to capability.deregistered");
                return;
            }
        };

        info!("Listening for capability.registered and capability.deregistered events");

        loop {
            tokio::select! {
                Some(msg) = reg_sub.next() => {
                    let reg: CapabilityRegistered = match serde_json::from_slice(&msg.payload) {
                        Ok(r) => r,
                        Err(err) => {
                            warn!(%err, "Failed to parse capability.registered event");
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
                Some(msg) = dereg_sub.next() => {
                    let dereg: CapabilityDeregistered = match serde_json::from_slice(&msg.payload) {
                        Ok(r) => r,
                        Err(err) => {
                            warn!(%err, "Failed to parse capability.deregistered event");
                            continue;
                        }
                    };

                    info!(tool_id = %dereg.tool_id, "Command capability deregistered");

                    if registry.remove_by_capability_id(&dereg.tool_id).await {
                        set_telegram_commands(&bot, &registry).await;
                    }
                }
                else => break,
            }
        }
    });
}
