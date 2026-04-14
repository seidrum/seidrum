use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use seidrum_common::events::{
    ChannelInbound, EventEnvelope, PluginRegister, ToolCallRequest, ToolCallResponse,
};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(name = "seidrum-calendar", about = "Seidrum Google Calendar plugin")]
struct Cli {
    /// Bus server URL
    #[arg(long, env = "BUS_URL", default_value = "ws://127.0.0.1:9000")]
    bus_url: String,

    /// Google API key (falls back to OpenClaw auth-profiles.json)
    #[arg(long, env = "GOOGLE_API_KEY", default_value = "")]
    google_api_key: Option<String>,

    /// Google Calendar ID
    #[arg(long, env = "GOOGLE_CALENDAR_ID", default_value = "primary")]
    calendar_id: String,

    /// Calendar polling interval in seconds
    #[arg(long, env = "CALENDAR_POLL_INTERVAL", default_value = "300")]
    poll_interval: u64,
}

/// Resolve the Google API key from env var or OpenClaw auth-profiles.json fallback.
fn resolve_google_api_key(cli_key: &Option<String>) -> Result<String> {
    // 1. CLI arg / env var
    if let Some(key) = cli_key {
        if !key.is_empty() {
            info!("Using Google API key from GOOGLE_API_KEY env var");
            return Ok(key.clone());
        }
    }

    // 2. OpenClaw auth-profiles.json fallback
    let auth_path = dirs::home_dir()
        .map(|h| h.join(".openclaw/agents/main/agent/auth-profiles.json"))
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;

    let content = std::fs::read_to_string(&auth_path).map_err(|e| {
        anyhow::anyhow!(
            "Failed to read OpenClaw auth-profiles.json at {}: {}",
            auth_path.display(),
            e
        )
    })?;

    let profiles: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse OpenClaw auth-profiles.json: {}", e))?;

    let key = profiles
        .get("profiles")
        .and_then(|p| p.get("google:default"))
        .and_then(|v| v.get("key"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!("No google:default.key found in OpenClaw auth-profiles.json")
        })?;

    info!("Using Google API key from OpenClaw auth-profiles.json");
    Ok(key.to_string())
}

/// Google Calendar API event response structures.
#[derive(serde::Deserialize, Debug)]
struct CalendarEventsResponse {
    items: Option<Vec<CalendarEvent>>,
}

#[derive(serde::Deserialize, Debug)]
struct CalendarEvent {
    id: Option<String>,
    summary: Option<String>,
    description: Option<String>,
    location: Option<String>,
    start: Option<CalendarDateTime>,
    end: Option<CalendarDateTime>,
    organizer: Option<CalendarOrganizer>,
}

#[derive(serde::Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct CalendarDateTime {
    date_time: Option<String>,
    date: Option<String>,
}

#[derive(serde::Deserialize, Debug)]
struct CalendarOrganizer {
    email: Option<String>,
}

impl CalendarDateTime {
    fn display(&self) -> String {
        self.date_time
            .as_deref()
            .or(self.date.as_deref())
            .unwrap_or("unknown")
            .to_string()
    }
}

/// Create calendar event request.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct CalendarEventCreate {
    pub summary: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start: String,
    pub end: String,
}

/// Poll Google Calendar for upcoming events and publish new ones.
async fn poll_calendar(
    client: &reqwest::Client,
    api_key: &str,
    calendar_id: &str,
    nats: &seidrum_common::bus_client::BusClient,
    seen: &Arc<Mutex<HashSet<String>>>,
) -> Result<()> {
    let now = chrono::Utc::now();
    let time_max = now + chrono::Duration::days(7);

    let url = format!(
        "https://www.googleapis.com/calendar/v3/calendars/{}/events",
        calendar_id
    );

    let resp = client
        .get(&url)
        .query(&[
            ("key", api_key),
            ("timeMin", &now.to_rfc3339()),
            ("timeMax", &time_max.to_rfc3339()),
            ("singleEvents", "true"),
            ("orderBy", "startTime"),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "Google Calendar API error ({}): {}",
            status,
            body
        ));
    }

    let events_resp: CalendarEventsResponse = resp.json().await?;

    let items = match events_resp.items {
        Some(items) => items,
        None => return Ok(()),
    };

    let mut seen_lock = seen.lock().await;

    for event in items {
        let event_id = match &event.id {
            Some(id) => id.clone(),
            None => continue,
        };

        if seen_lock.contains(&event_id) {
            continue;
        }

        let summary = event.summary.as_deref().unwrap_or("(No title)");
        let description = event.description.as_deref().unwrap_or("");
        let location = event.location.as_deref().unwrap_or("");
        let start_str = event
            .start
            .as_ref()
            .map(|s| s.display())
            .unwrap_or_default();
        let end_str = event.end.as_ref().map(|s| s.display()).unwrap_or_default();
        let organizer = event
            .organizer
            .as_ref()
            .and_then(|o| o.email.as_deref())
            .unwrap_or("");

        let text = format!(
            "Event: {}\nWhen: {} - {}\nLocation: {}\nDescription: {}",
            summary, start_str, end_str, location, description
        );

        let mut metadata = HashMap::new();
        metadata.insert("calendar_id".to_string(), calendar_id.to_string());
        metadata.insert("event_id".to_string(), event_id.clone());
        metadata.insert("start".to_string(), start_str);
        metadata.insert("end".to_string(), end_str);
        metadata.insert("location".to_string(), location.to_string());
        metadata.insert("organizer".to_string(), organizer.to_string());

        let inbound = ChannelInbound {
            platform: "calendar".to_string(),
            user_id: "google-calendar".to_string(),
            chat_id: event_id.clone(),
            text,
            reply_to: None,
            attachments: vec![],
            metadata,
        };

        let envelope =
            EventEnvelope::new("channel.calendar.inbound", "calendar", None, None, &inbound)?;

        let bytes = serde_json::to_vec(&envelope)?;
        nats.publish_bytes("channel.calendar.inbound", bytes)
            .await?;

        seen_lock.insert(event_id.clone());
        info!(event_id = %event_id, summary = %summary, "Published channel.calendar.inbound");
    }

    Ok(())
}

/// Create a Google Calendar event via the API.
async fn create_calendar_event(
    client: &reqwest::Client,
    api_key: &str,
    calendar_id: &str,
    event: &CalendarEventCreate,
) -> Result<()> {
    let url = format!(
        "https://www.googleapis.com/calendar/v3/calendars/{}/events",
        calendar_id
    );

    let body = serde_json::json!({
        "summary": event.summary,
        "description": event.description,
        "location": event.location,
        "start": { "dateTime": event.start },
        "end": { "dateTime": event.end },
    });

    let resp = client
        .post(&url)
        .query(&[("key", api_key)])
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let resp_body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "Google Calendar create event error ({}): {}",
            status,
            resp_body
        ));
    }

    info!(summary = %event.summary, "Created Google Calendar event");
    Ok(())
}

/// Search Google Calendar events matching a query within a number of days ahead.
async fn search_calendar_events(
    client: &reqwest::Client,
    api_key: &str,
    calendar_id: &str,
    query: &str,
    days_ahead: i64,
) -> Result<Vec<serde_json::Value>> {
    let now = chrono::Utc::now();
    let time_max = now + chrono::Duration::days(days_ahead);

    let url = format!(
        "https://www.googleapis.com/calendar/v3/calendars/{}/events",
        calendar_id
    );

    let resp = client
        .get(&url)
        .query(&[
            ("key", api_key),
            ("timeMin", &now.to_rfc3339()),
            ("timeMax", &time_max.to_rfc3339()),
            ("singleEvents", "true"),
            ("orderBy", "startTime"),
            ("q", query),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "Google Calendar API error ({}): {}",
            status,
            body
        ));
    }

    let events_resp: CalendarEventsResponse = resp.json().await?;

    let items = match events_resp.items {
        Some(items) => items,
        None => return Ok(vec![]),
    };

    let results: Vec<serde_json::Value> = items
        .iter()
        .map(|event| {
            serde_json::json!({
                "id": event.id,
                "summary": event.summary,
                "description": event.description,
                "location": event.location,
                "start": event.start.as_ref().map(|s| s.display()),
                "end": event.end.as_ref().map(|s| s.display()),
                "organizer": event.organizer.as_ref().and_then(|o| o.email.as_deref()),
            })
        })
        .collect();

    Ok(results)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    info!(
        bus_url = %cli.bus_url,
        calendar_id = %cli.calendar_id,
        poll_interval = cli.poll_interval,
        "Starting seidrum-calendar plugin..."
    );

    let api_key = resolve_google_api_key(&cli.google_api_key)?;

    // Connect to NATS
    let nats = seidrum_common::bus_client::BusClient::connect(&cli.bus_url, "calendar").await?;
    info!("Connected to bus at {}", cli.bus_url);

    // Register plugin
    let register = PluginRegister {
        id: "calendar".to_string(),
        name: "Google Calendar".to_string(),
        version: "0.1.0".to_string(),
        description: "Google Calendar sync — ingests events and can create new ones".to_string(),
        consumes: vec!["calendar.event.create".to_string()],
        produces: vec!["channel.calendar.inbound".to_string()],
        health_subject: "plugin.calendar.health".to_string(),
        consumed_event_types: vec![],
        produced_event_types: vec![],
        config_schema: None,
    };
    let register_envelope =
        EventEnvelope::new("plugin.register", "calendar", None, None, &register)?;
    let register_bytes = serde_json::to_vec(&register_envelope)?;
    nats.publish_bytes("plugin.register", register_bytes)
        .await?;
    info!("Published plugin.register event");

    // Register tools with the kernel's tool registry
    let search_tool = serde_json::json!({
        "tool_id": "search-calendar",
        "plugin_id": "calendar",
        "name": "Search Calendar",
        "summary_md": "Search upcoming Google Calendar events by query text within a number of days ahead.",
        "manual_md": "# Search Calendar\n\nSearch upcoming events from Google Calendar.\n\n## Parameters\n- `query` (string): Text to search for in event summaries/descriptions\n- `days_ahead` (integer, default 7): Number of days ahead to search\n\n## Returns\nA list of matching calendar events with summary, start, end, location, and description.",
        "parameters": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Text to search for in event summaries and descriptions"
                },
                "days_ahead": {
                    "type": "integer",
                    "description": "Number of days ahead to search (default 7)"
                }
            },
            "required": ["query"]
        },
        "call_subject": "capability.call.calendar",
        "kind": "tool"
    });

    nats.publish_bytes("capability.register", serde_json::to_vec(&search_tool)?)
        .await?;
    info!("Tool 'search-calendar' registered with kernel");

    let create_tool = serde_json::json!({
        "tool_id": "create-calendar-event",
        "plugin_id": "calendar",
        "name": "Create Calendar Event",
        "summary_md": "Create a new Google Calendar event with summary, start/end times, and optional description/location.",
        "manual_md": "# Create Calendar Event\n\nCreate a new event on Google Calendar.\n\n## Parameters\n- `summary` (string, required): Event title\n- `start` (string, required): ISO 8601 datetime for event start\n- `end` (string, required): ISO 8601 datetime for event end\n- `description` (string, optional): Event description\n- `location` (string, optional): Event location\n\n## Returns\nConfirmation that the event was created.",
        "parameters": {
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "Event title"
                },
                "start": {
                    "type": "string",
                    "description": "ISO 8601 datetime for event start"
                },
                "end": {
                    "type": "string",
                    "description": "ISO 8601 datetime for event end"
                },
                "description": {
                    "type": "string",
                    "description": "Event description"
                },
                "location": {
                    "type": "string",
                    "description": "Event location"
                }
            },
            "required": ["summary", "start", "end"]
        },
        "call_subject": "capability.call.calendar",
        "kind": "tool"
    });

    nats.publish_bytes("capability.register", serde_json::to_vec(&create_tool)?)
        .await?;
    info!("Tool 'create-calendar-event' registered with kernel");

    // Subscribe to event creation requests
    let mut create_sub = nats.subscribe("calendar.event.create").await?;
    info!("Subscribed to calendar.event.create");

    // Track seen event IDs to avoid duplicates
    let seen: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    // Spawn calendar polling task
    let nats_poll = nats.clone();
    let poll_api_key = api_key.clone();
    let poll_calendar_id = cli.calendar_id.clone();
    let poll_interval = cli.poll_interval;
    let seen_poll = seen.clone();

    let http_client = reqwest::Client::new();
    let poll_client = http_client.clone();

    let poll_handle = tokio::spawn(async move {
        loop {
            if let Err(err) = poll_calendar(
                &poll_client,
                &poll_api_key,
                &poll_calendar_id,
                &nats_poll,
                &seen_poll,
            )
            .await
            {
                error!(%err, "Calendar poll error");
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(poll_interval)).await;
        }
    });

    // Spawn event creation handler
    let create_api_key = api_key.clone();
    let create_calendar_id = cli.calendar_id.clone();
    let create_client = http_client.clone();

    let create_handle = tokio::spawn(async move {
        while let Some(msg) = create_sub.next().await {
            let envelope: EventEnvelope = match serde_json::from_slice(&msg.payload) {
                Ok(e) => e,
                Err(err) => {
                    error!(%err, "Failed to parse calendar.event.create EventEnvelope");
                    continue;
                }
            };

            let create_req: CalendarEventCreate = match serde_json::from_value(envelope.payload) {
                Ok(c) => c,
                Err(err) => {
                    error!(%err, "Failed to parse CalendarEventCreate payload");
                    continue;
                }
            };

            if let Err(err) = create_calendar_event(
                &create_client,
                &create_api_key,
                &create_calendar_id,
                &create_req,
            )
            .await
            {
                error!(%err, "Failed to create calendar event");
            }
        }
    });

    // Subscribe to capability.call.calendar for tool dispatch
    let mut tool_sub = nats.subscribe("capability.call.calendar").await?;
    info!("Subscribed to capability.call.calendar");

    let tool_api_key = api_key.clone();
    let tool_calendar_id = cli.calendar_id.clone();
    let tool_nats = nats.clone();
    let tool_client = http_client;

    let tool_handle = tokio::spawn(async move {
        while let Some(msg) = tool_sub.next().await {
            let reply = match msg.reply {
                Some(ref r) => r.clone(),
                None => {
                    warn!("Received capability.call.calendar without reply subject, skipping");
                    continue;
                }
            };

            let tool_request: ToolCallRequest = match serde_json::from_slice(&msg.payload) {
                Ok(r) => r,
                Err(err) => {
                    warn!(%err, "Failed to deserialize ToolCallRequest");
                    let error_response = ToolCallResponse {
                        tool_id: "unknown".to_string(),
                        result: serde_json::json!({"error": format!("Invalid request: {}", err)}),
                        is_error: true,
                    };
                    if let Err(e) = tool_nats
                        .publish_bytes(
                            reply,
                            serde_json::to_vec(&error_response).unwrap_or_default(),
                        )
                        .await
                    {
                        error!(%e, "Failed to publish error reply");
                    }
                    continue;
                }
            };

            let tool_response = match tool_request.tool_id.as_str() {
                "search-calendar" => {
                    let query = tool_request
                        .arguments
                        .get("query")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let days_ahead = tool_request
                        .arguments
                        .get("days_ahead")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(7);

                    info!(query = %query, days_ahead = days_ahead, "Searching calendar");

                    match search_calendar_events(
                        &tool_client,
                        &tool_api_key,
                        &tool_calendar_id,
                        &query,
                        days_ahead,
                    )
                    .await
                    {
                        Ok(events) => ToolCallResponse {
                            tool_id: tool_request.tool_id,
                            result: serde_json::json!({"events": events}),
                            is_error: false,
                        },
                        Err(err) => ToolCallResponse {
                            tool_id: tool_request.tool_id,
                            result: serde_json::json!({"error": format!("{}", err)}),
                            is_error: true,
                        },
                    }
                }
                "create-calendar-event" => {
                    let summary = tool_request
                        .arguments
                        .get("summary")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let start = tool_request
                        .arguments
                        .get("start")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let end = tool_request
                        .arguments
                        .get("end")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let description = tool_request
                        .arguments
                        .get("description")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let location = tool_request
                        .arguments
                        .get("location")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());

                    let create_req = CalendarEventCreate {
                        summary: summary.clone(),
                        description,
                        location,
                        start,
                        end,
                    };

                    info!(summary = %summary, "Creating calendar event via tool");

                    match create_calendar_event(
                        &tool_client,
                        &tool_api_key,
                        &tool_calendar_id,
                        &create_req,
                    )
                    .await
                    {
                        Ok(()) => ToolCallResponse {
                            tool_id: tool_request.tool_id,
                            result: serde_json::json!({"status": "created", "summary": create_req.summary}),
                            is_error: false,
                        },
                        Err(err) => ToolCallResponse {
                            tool_id: tool_request.tool_id,
                            result: serde_json::json!({"error": format!("{}", err)}),
                            is_error: true,
                        },
                    }
                }
                other => ToolCallResponse {
                    tool_id: other.to_string(),
                    result: serde_json::json!({"error": format!("Unknown tool_id: {}", other)}),
                    is_error: true,
                },
            };

            if let Err(err) = tool_nats
                .publish_bytes(
                    reply,
                    serde_json::to_vec(&tool_response).unwrap_or_default(),
                )
                .await
            {
                error!(%err, "Failed to publish tool call reply");
            }
        }
    });

    tokio::select! {
        result = poll_handle => {
            match result {
                Ok(()) => info!("Calendar poller stopped"),
                Err(err) => error!(%err, "Calendar poller panicked"),
            }
        }
        result = create_handle => {
            match result {
                Ok(()) => info!("Create handler stopped"),
                Err(err) => error!(%err, "Create handler panicked"),
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

    #[test]
    fn test_calendar_event_create_roundtrip() {
        let event = CalendarEventCreate {
            summary: "Team standup".to_string(),
            description: Some("Daily sync".to_string()),
            location: Some("Zoom".to_string()),
            start: "2025-01-15T09:00:00Z".to_string(),
            end: "2025-01-15T09:30:00Z".to_string(),
        };

        let json = serde_json::to_string(&event).unwrap();
        let deserialized: CalendarEventCreate = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.summary, "Team standup");
        assert_eq!(deserialized.start, "2025-01-15T09:00:00Z");
    }

    #[test]
    fn test_calendar_inbound_event() {
        let mut metadata = HashMap::new();
        metadata.insert("calendar_id".to_string(), "primary".to_string());
        metadata.insert("event_id".to_string(), "evt-123".to_string());
        metadata.insert("start".to_string(), "2025-01-15T09:00:00Z".to_string());
        metadata.insert("end".to_string(), "2025-01-15T09:30:00Z".to_string());
        metadata.insert("location".to_string(), "Zoom".to_string());
        metadata.insert("organizer".to_string(), "alice@example.com".to_string());

        let inbound = ChannelInbound {
            platform: "calendar".to_string(),
            user_id: "google-calendar".to_string(),
            chat_id: "evt-123".to_string(),
            text: "Event: Team standup\nWhen: 2025-01-15T09:00:00Z - 2025-01-15T09:30:00Z\nLocation: Zoom\nDescription: Daily sync".to_string(),
            reply_to: None,
            attachments: vec![],
            metadata,
        };

        let envelope =
            EventEnvelope::new("channel.calendar.inbound", "calendar", None, None, &inbound)
                .unwrap();

        assert_eq!(envelope.event_type, "channel.calendar.inbound");
        assert_eq!(envelope.source, "calendar");

        let recovered: ChannelInbound = serde_json::from_value(envelope.payload).unwrap();
        assert_eq!(recovered.platform, "calendar");
        assert_eq!(recovered.chat_id, "evt-123");
    }

    #[test]
    fn test_calendar_datetime_display() {
        let dt = CalendarDateTime {
            date_time: Some("2025-01-15T09:00:00Z".to_string()),
            date: None,
        };
        assert_eq!(dt.display(), "2025-01-15T09:00:00Z");

        let dt_date = CalendarDateTime {
            date_time: None,
            date: Some("2025-01-15".to_string()),
        };
        assert_eq!(dt_date.display(), "2025-01-15");

        let dt_none = CalendarDateTime {
            date_time: None,
            date: None,
        };
        assert_eq!(dt_none.display(), "unknown");
    }
}
