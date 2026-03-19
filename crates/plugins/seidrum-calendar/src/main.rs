use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use futures::StreamExt;
use seidrum_common::events::{ChannelInbound, EventEnvelope, PluginRegister};
use tokio::sync::Mutex;
use tracing::{error, info};

#[derive(Parser)]
#[command(name = "seidrum-calendar", about = "Seidrum Google Calendar plugin")]
struct Cli {
    /// NATS server URL
    #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4222")]
    nats_url: String,

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

    let content = std::fs::read_to_string(&auth_path)
        .map_err(|e| anyhow::anyhow!("Failed to read OpenClaw auth-profiles.json at {}: {}", auth_path.display(), e))?;

    let profiles: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse OpenClaw auth-profiles.json: {}", e))?;

    let key = profiles
        .get("profiles")
        .and_then(|p| p.get("google:default"))
        .and_then(|v| v.get("key"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("No google:default.key found in OpenClaw auth-profiles.json"))?;

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
    nats: &async_nats::Client,
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
        let start_str = event.start.as_ref().map(|s| s.display()).unwrap_or_default();
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

        let envelope = EventEnvelope::new(
            "channel.calendar.inbound",
            "calendar",
            None,
            None,
            &inbound,
        )?;

        let bytes = serde_json::to_vec(&envelope)?;
        nats.publish("channel.calendar.inbound", bytes.into())
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    info!(
        nats_url = %cli.nats_url,
        calendar_id = %cli.calendar_id,
        poll_interval = cli.poll_interval,
        "Starting seidrum-calendar plugin..."
    );

    let api_key = resolve_google_api_key(&cli.google_api_key)?;

    // Connect to NATS
    let nats = async_nats::connect(&cli.nats_url).await?;
    info!("Connected to NATS at {}", cli.nats_url);

    // Register plugin
    let register = PluginRegister {
        id: "calendar".to_string(),
        name: "Google Calendar".to_string(),
        version: "0.1.0".to_string(),
        description: "Google Calendar sync — ingests events and can create new ones".to_string(),
        consumes: vec!["calendar.event.create".to_string()],
        produces: vec!["channel.calendar.inbound".to_string()],
        health_subject: "plugin.calendar.health".to_string(),
    };
    let register_envelope = EventEnvelope::new(
        "plugin.register",
        "calendar",
        None,
        None,
        &register,
    )?;
    let register_bytes = serde_json::to_vec(&register_envelope)?;
    nats.publish("plugin.register", register_bytes.into()).await?;
    info!("Published plugin.register event");

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
    let create_client = http_client;

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

        let envelope = EventEnvelope::new(
            "channel.calendar.inbound",
            "calendar",
            None,
            None,
            &inbound,
        )
        .unwrap();

        assert_eq!(envelope.event_type, "channel.calendar.inbound");
        assert_eq!(envelope.source, "calendar");

        let recovered: ChannelInbound =
            serde_json::from_value(envelope.payload).unwrap();
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
