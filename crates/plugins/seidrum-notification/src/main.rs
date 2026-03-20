use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use seidrum_common::events::{
    ChannelOutbound, EventEnvelope, PluginRegister, SystemHealth, TaskCompleted, TaskCreated,
    ToolCallRequest, ToolCallResponse,
};
use tracing::{error, info, warn};

const PLUGIN_ID: &str = "notification";
const PLUGIN_NAME: &str = "Notification";
const PLUGIN_VERSION: &str = "0.1.0";

#[derive(Parser, Debug)]
#[command(name = "seidrum-notification")]
#[command(about = "Cross-channel notification plugin")]
struct Args {
    /// NATS server URL
    #[arg(long, env = "NATS_URL", default_value = "nats://127.0.0.1:4222")]
    nats_url: String,

    /// Default notification channel: "telegram" or "cli"
    #[arg(long, env = "NOTIFICATION_CHANNEL", default_value = "telegram")]
    notification_channel: String,

    /// Notification level: "all", "important", "critical"
    #[arg(long, env = "NOTIFICATION_LEVEL", default_value = "all")]
    notification_level: String,
}

/// Notification importance level for filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Importance {
    Info,
    Important,
    Critical,
}

impl Importance {
    fn from_level_filter(level: &str) -> Self {
        match level {
            "critical" => Importance::Critical,
            "important" => Importance::Important,
            _ => Importance::Info,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    info!(plugin = PLUGIN_ID, "Starting notification plugin");

    // Connect to NATS
    let client = async_nats::connect(&args.nats_url)
        .await
        .context("Failed to connect to NATS")?;

    info!(url = %args.nats_url, "Connected to NATS");

    // Register plugin
    let register = PluginRegister {
        id: PLUGIN_ID.to_string(),
        name: PLUGIN_NAME.to_string(),
        version: PLUGIN_VERSION.to_string(),
        description: "Cross-channel notification plugin".to_string(),
        consumes: vec![
            "task.created".to_string(),
            "task.completed.*".to_string(),
            "system.health".to_string(),
        ],
        produces: vec![
            "channel.telegram.outbound".to_string(),
            "channel.cli.outbound".to_string(),
        ],
        health_subject: format!("plugin.{}.health", PLUGIN_ID),
    };

    let register_envelope = EventEnvelope::new(
        "plugin.register",
        PLUGIN_ID,
        None,
        None,
        &register,
    )?;

    client
        .publish(
            "plugin.register",
            serde_json::to_vec(&register_envelope)?.into(),
        )
        .await
        .context("Failed to publish plugin.register")?;

    info!("Plugin registered");

    // Register tool with the kernel's tool registry
    let send_notification_tool = serde_json::json!({
        "tool_id": "send-notification",
        "plugin_id": PLUGIN_ID,
        "name": "Send Notification",
        "summary_md": "Send a notification message to a channel (telegram or cli) with configurable priority.",
        "manual_md": "# Send Notification\n\nSend a notification to a configured channel.\n\n## Parameters\n- `message` (string, required): The notification message text\n- `channel` (string, default \"telegram\"): Target channel — \"telegram\" or \"cli\"\n- `priority` (string, default \"normal\"): Priority level — \"normal\", \"important\", or \"critical\"\n\n## Returns\nConfirmation that the notification was sent.",
        "parameters": {
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The notification message text"
                },
                "channel": {
                    "type": "string",
                    "description": "Target channel: telegram or cli (default: telegram)"
                },
                "priority": {
                    "type": "string",
                    "description": "Priority level: normal, important, or critical (default: normal)"
                }
            },
            "required": ["message"]
        },
        "call_subject": "capability.call.notification",
        "kind": "command"
    });

    client
        .publish("capability.register", serde_json::to_vec(&send_notification_tool)?.into())
        .await
        .context("Failed to publish capability.register for send-notification")?;

    info!("Tool 'send-notification' registered with kernel");

    // Subscribe to all consumed subjects
    let mut task_created_sub = client
        .subscribe("task.created")
        .await
        .context("Failed to subscribe to task.created")?;

    let mut task_completed_sub = client
        .subscribe("task.completed.*")
        .await
        .context("Failed to subscribe to task.completed.*")?;

    let mut system_health_sub = client
        .subscribe("system.health")
        .await
        .context("Failed to subscribe to system.health")?;

    // Subscribe to capability.call.notification for capability dispatch
    let mut tool_sub = client
        .subscribe("capability.call.notification")
        .await
        .context("Failed to subscribe to capability.call.notification")?;

    info!("Subscribed to task.created, task.completed.*, system.health, capability.call.notification");

    let min_importance = Importance::from_level_filter(&args.notification_level);
    let channel = args.notification_channel.clone();

    loop {
        tokio::select! {
            Some(msg) = task_created_sub.next() => {
                if min_importance > Importance::Info {
                    continue;
                }
                if let Err(err) = handle_task_created(&client, &msg.payload, &channel).await {
                    error!(%err, "Failed to handle task.created");
                }
            }
            Some(msg) = task_completed_sub.next() => {
                if min_importance > Importance::Info {
                    continue;
                }
                if let Err(err) = handle_task_completed(&client, &msg.payload, &channel).await {
                    error!(%err, "Failed to handle task.completed");
                }
            }
            Some(msg) = system_health_sub.next() => {
                if let Err(err) = handle_system_health(&client, &msg.payload, &channel, min_importance).await {
                    error!(%err, "Failed to handle system.health");
                }
            }
            Some(msg) = tool_sub.next() => {
                let reply = match msg.reply {
                    Some(ref r) => r.clone(),
                    None => {
                        warn!("Received capability.call.notification without reply subject, skipping");
                        continue;
                    }
                };

                let tool_request: ToolCallRequest = match serde_json::from_slice(&msg.payload) {
                    Ok(r) => r,
                    Err(err) => {
                        warn!(%err, "Failed to deserialize ToolCallRequest");
                        let error_response = ToolCallResponse {
                            tool_id: "send-notification".to_string(),
                            result: serde_json::json!({"error": format!("Invalid request: {}", err)}),
                            is_error: true,
                        };
                        if let Err(e) = client
                            .publish(reply, serde_json::to_vec(&error_response).unwrap_or_default().into())
                            .await
                        {
                            error!(%e, "Failed to publish error reply");
                        }
                        continue;
                    }
                };

                let tool_response = match tool_request.tool_id.as_str() {
                    "send-notification" => {
                        let message = tool_request.arguments.get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let target_channel = tool_request.arguments.get("channel")
                            .and_then(|v| v.as_str())
                            .unwrap_or("telegram")
                            .to_string();
                        let priority = tool_request.arguments.get("priority")
                            .and_then(|v| v.as_str())
                            .unwrap_or("normal")
                            .to_string();

                        info!(channel = %target_channel, priority = %priority, "Sending notification via tool");

                        let format = if target_channel == "telegram" { "markdown" } else { "plain" };

                        match send_notification(
                            &client,
                            &target_channel,
                            &message,
                            format,
                            tool_request.correlation_id.clone(),
                            None,
                        ).await {
                            Ok(()) => ToolCallResponse {
                                tool_id: tool_request.tool_id,
                                result: serde_json::json!({
                                    "status": "sent",
                                    "channel": target_channel,
                                    "priority": priority,
                                }),
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

                if let Err(err) = client
                    .publish(reply, serde_json::to_vec(&tool_response).unwrap_or_default().into())
                    .await
                {
                    error!(%err, "Failed to publish tool call reply");
                }
            }
            else => break,
        }
    }

    Ok(())
}

async fn handle_task_created(
    nats: &async_nats::Client,
    payload: &[u8],
    channel: &str,
) -> Result<()> {
    let envelope: EventEnvelope = serde_json::from_slice(payload)
        .context("Failed to deserialize task.created envelope")?;

    let task: TaskCreated = serde_json::from_value(envelope.payload.clone())
        .context("Failed to deserialize TaskCreated payload")?;

    info!(task_key = %task.task_key, title = %task.title, "Task created notification");

    let (text, format) = format_task_created(&task, channel);

    send_notification(nats, channel, &text, &format, envelope.correlation_id, envelope.scope).await
}

async fn handle_task_completed(
    nats: &async_nats::Client,
    payload: &[u8],
    channel: &str,
) -> Result<()> {
    let envelope: EventEnvelope = serde_json::from_slice(payload)
        .context("Failed to deserialize task.completed envelope")?;

    let task: TaskCompleted = serde_json::from_value(envelope.payload.clone())
        .context("Failed to deserialize TaskCompleted payload")?;

    info!(task_key = %task.task_key, "Task completed notification");

    let (text, format) = format_task_completed(&task, channel);

    send_notification(nats, channel, &text, &format, envelope.correlation_id, envelope.scope).await
}

async fn handle_system_health(
    nats: &async_nats::Client,
    payload: &[u8],
    channel: &str,
    min_importance: Importance,
) -> Result<()> {
    let envelope: EventEnvelope = serde_json::from_slice(payload)
        .context("Failed to deserialize system.health envelope")?;

    let health: SystemHealth = serde_json::from_value(envelope.payload.clone())
        .context("Failed to deserialize SystemHealth payload")?;

    // Only alert if something is unhealthy
    let has_issues = !health.nats_connected || !health.arangodb_connected;
    if !has_issues {
        return Ok(());
    }

    // System health issues are at least important, critical if DB is down
    let event_importance = if !health.arangodb_connected || !health.nats_connected {
        Importance::Critical
    } else {
        Importance::Important
    };

    if event_importance < min_importance {
        return Ok(());
    }

    warn!(
        nats_connected = health.nats_connected,
        arangodb_connected = health.arangodb_connected,
        "System health issue detected"
    );

    let (text, format) = format_health_alert(&health, channel);

    send_notification(nats, channel, &text, &format, envelope.correlation_id, envelope.scope).await
}

async fn send_notification(
    nats: &async_nats::Client,
    channel: &str,
    text: &str,
    format: &str,
    correlation_id: Option<String>,
    scope: Option<String>,
) -> Result<()> {
    let outbound = ChannelOutbound {
        platform: channel.to_string(),
        chat_id: "default".to_string(),
        text: text.to_string(),
        format: format.to_string(),
        reply_to: None,
        actions: vec![],
    };

    let subject = format!("channel.{}.outbound", channel);

    let envelope = EventEnvelope::new(
        &subject,
        PLUGIN_ID,
        correlation_id,
        scope,
        &outbound,
    )?;

    nats.publish(
        subject.clone(),
        serde_json::to_vec(&envelope)?.into(),
    )
    .await
    .context("Failed to publish notification")?;

    info!(channel = %channel, subject = %subject, "Notification sent");

    Ok(())
}

/// Format a task-created notification for the given channel.
fn format_task_created(task: &TaskCreated, channel: &str) -> (String, String) {
    if channel == "telegram" {
        let priority_emoji = match task.priority.as_str() {
            "critical" => "\u{1f534}",
            "high" => "\u{1f7e0}",
            "medium" => "\u{1f7e1}",
            "low" => "\u{1f7e2}",
            _ => "\u{26aa}",
        };
        let mut text = format!(
            "\u{1f4cb} *New Task Created*\n\n{} *Priority:* {}\n*Title:* {}\n*Scope:* {}",
            priority_emoji, task.priority, task.title, task.scope
        );
        if let Some(ref desc) = task.description {
            text.push_str(&format!("\n*Description:* {}", desc));
        }
        if let Some(ref agent) = task.assigned_agent {
            text.push_str(&format!("\n*Assigned to:* {}", agent));
        }
        if let Some(ref due) = task.due_date {
            text.push_str(&format!("\n*Due:* {}", due.format("%Y-%m-%d %H:%M UTC")));
        }
        (text, "markdown".to_string())
    } else {
        let mut text = format!(
            "[TASK CREATED] {} (priority: {}, scope: {})",
            task.title, task.priority, task.scope
        );
        if let Some(ref desc) = task.description {
            text.push_str(&format!(" - {}", desc));
        }
        (text, "plain".to_string())
    }
}

/// Format a task-completed notification for the given channel.
fn format_task_completed(task: &TaskCompleted, channel: &str) -> (String, String) {
    if channel == "telegram" {
        let mut text = format!(
            "\u{2705} *Task Completed*\n\n*Task:* {}",
            task.task_key
        );
        if let Some(ref result) = task.result {
            text.push_str(&format!("\n*Result:* {}", result));
        }
        let duration = format_duration_ms(task.duration_ms);
        text.push_str(&format!("\n*Duration:* {}", duration));
        (text, "markdown".to_string())
    } else {
        let mut text = format!("[TASK COMPLETED] {}", task.task_key);
        if let Some(ref result) = task.result {
            text.push_str(&format!(" - Result: {}", result));
        }
        text.push_str(&format!(" (took {})", format_duration_ms(task.duration_ms)));
        (text, "plain".to_string())
    }
}

/// Format a system health alert for the given channel.
fn format_health_alert(health: &SystemHealth, channel: &str) -> (String, String) {
    let mut issues = Vec::new();
    if !health.nats_connected {
        issues.push("NATS disconnected");
    }
    if !health.arangodb_connected {
        issues.push("ArangoDB disconnected");
    }

    if channel == "telegram" {
        let text = format!(
            "\u{1f6a8} *System Health Alert*\n\n*Issues:*\n{}\n\n*Active plugins:* {}\n*Uptime:* {}s",
            issues.iter().map(|i| format!("\u{274c} {}", i)).collect::<Vec<_>>().join("\n"),
            health.active_plugins.len(),
            health.uptime_seconds,
        );
        (text, "markdown".to_string())
    } else {
        let text = format!(
            "[ALERT] System health issues: {} | Active plugins: {} | Uptime: {}s",
            issues.join(", "),
            health.active_plugins.len(),
            health.uptime_seconds,
        );
        (text, "plain".to_string())
    }
}

/// Format milliseconds into a human-readable duration string.
fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{}ms", ms)
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let minutes = ms / 60_000;
        let seconds = (ms % 60_000) / 1000;
        format!("{}m {}s", minutes, seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_format_task_created_telegram() {
        let task = TaskCreated {
            task_key: "tasks/123".to_string(),
            title: "Review PR".to_string(),
            description: Some("Check the new feature".to_string()),
            priority: "high".to_string(),
            assigned_agent: Some("main-agent".to_string()),
            due_date: None,
            callback_channel: None,
            scope: "engineering".to_string(),
        };

        let (text, format) = format_task_created(&task, "telegram");
        assert_eq!(format, "markdown");
        assert!(text.contains("New Task Created"));
        assert!(text.contains("Review PR"));
        assert!(text.contains("high"));
    }

    #[test]
    fn test_format_task_created_cli() {
        let task = TaskCreated {
            task_key: "tasks/123".to_string(),
            title: "Review PR".to_string(),
            description: None,
            priority: "medium".to_string(),
            assigned_agent: None,
            due_date: None,
            callback_channel: None,
            scope: "personal".to_string(),
        };

        let (text, format) = format_task_created(&task, "cli");
        assert_eq!(format, "plain");
        assert!(text.contains("[TASK CREATED]"));
        assert!(text.contains("Review PR"));
    }

    #[test]
    fn test_format_task_completed_telegram() {
        let task = TaskCompleted {
            task_key: "tasks/456".to_string(),
            result: Some("All tests passed".to_string()),
            duration_ms: 125_000,
            callback_channel: None,
        };

        let (text, format) = format_task_completed(&task, "telegram");
        assert_eq!(format, "markdown");
        assert!(text.contains("Task Completed"));
        assert!(text.contains("All tests passed"));
        assert!(text.contains("2m 5s"));
    }

    #[test]
    fn test_format_health_alert_telegram() {
        let health = SystemHealth {
            nats_connected: true,
            arangodb_connected: false,
            active_plugins: vec!["telegram".to_string()],
            active_agents: 1,
            uptime_seconds: 3600,
        };

        let (text, format) = format_health_alert(&health, "telegram");
        assert_eq!(format, "markdown");
        assert!(text.contains("System Health Alert"));
        assert!(text.contains("ArangoDB disconnected"));
    }

    #[test]
    fn test_format_health_alert_cli() {
        let health = SystemHealth {
            nats_connected: false,
            arangodb_connected: false,
            active_plugins: vec![],
            active_agents: 0,
            uptime_seconds: 10,
        };

        let (text, format) = format_health_alert(&health, "cli");
        assert_eq!(format, "plain");
        assert!(text.contains("[ALERT]"));
        assert!(text.contains("NATS disconnected"));
        assert!(text.contains("ArangoDB disconnected"));
    }

    #[test]
    fn test_format_duration_ms() {
        assert_eq!(format_duration_ms(500), "500ms");
        assert_eq!(format_duration_ms(1500), "1.5s");
        assert_eq!(format_duration_ms(125_000), "2m 5s");
    }

    #[test]
    fn test_importance_ordering() {
        assert!(Importance::Info < Importance::Important);
        assert!(Importance::Important < Importance::Critical);
    }

    #[test]
    fn test_importance_from_level_filter() {
        assert_eq!(Importance::from_level_filter("all"), Importance::Info);
        assert_eq!(Importance::from_level_filter("important"), Importance::Important);
        assert_eq!(Importance::from_level_filter("critical"), Importance::Critical);
    }
}
