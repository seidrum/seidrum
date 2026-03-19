use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use seidrum_common::events::{EventEnvelope, LlmResponse, PluginRegister};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{error, info, warn};

const PLUGIN_ID: &str = "task-detector";
const PLUGIN_NAME: &str = "Task Detector";
const PLUGIN_VERSION: &str = "0.1.0";

#[derive(Parser, Debug)]
#[command(name = "seidrum-task-detector")]
#[command(about = "Detects actionable tasks from LLM responses and creates them in the brain")]
struct Args {
    /// NATS server URL
    #[arg(long, env = "NATS_URL", default_value = "nats://127.0.0.1:4222")]
    nats_url: String,

    /// Anthropic API key
    #[arg(long, env = "ANTHROPIC_API_KEY")]
    anthropic_api_key: String,

    /// Model to use for task detection
    #[arg(long, env = "DETECTION_MODEL", default_value = "claude-haiku-4-5-20251001")]
    detection_model: String,
}

/// A task detected by the LLM.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct DetectedTask {
    /// Short descriptive title for the task
    title: String,
    /// Longer description of what needs to be done
    description: Option<String>,
    /// Priority: "low", "medium", "high", "urgent"
    priority: String,
    /// Scope context (e.g. "scope_career", "scope_personal")
    scope: Option<String>,
    /// Optional due date as ISO 8601 string
    due_date: Option<String>,
}

/// Anthropic Messages API request.
#[derive(Serialize, Debug)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<AnthropicMessage>,
}

#[derive(Serialize, Debug)]
struct AnthropicMessage {
    role: String,
    content: String,
}

/// Anthropic Messages API response.
#[derive(Deserialize, Debug)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
}

#[derive(Deserialize, Debug)]
struct AnthropicContent {
    text: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    info!(
        plugin = PLUGIN_ID,
        model = %args.detection_model,
        "Starting task detector plugin"
    );

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
        description: "Detects actionable tasks from LLM responses and creates them in the brain"
            .to_string(),
        consumes: vec!["llm.response".to_string()],
        produces: vec!["brain.task.upsert".to_string()],
        health_subject: format!("plugin.{PLUGIN_ID}.health"),
    };

    let register_envelope =
        EventEnvelope::new("plugin.register", PLUGIN_ID, None, None, &register)?;

    client
        .publish(
            "plugin.register",
            serde_json::to_vec(&register_envelope)?.into(),
        )
        .await
        .context("Failed to publish plugin.register")?;

    info!("Plugin registered");

    // Subscribe to llm.response
    let mut subscriber = client
        .subscribe("llm.response")
        .await
        .context("Failed to subscribe to llm.response")?;

    info!("Subscribed to llm.response, waiting for events...");

    let http_client = reqwest::Client::new();

    while let Some(msg) = subscriber.next().await {
        let envelope: EventEnvelope = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(err) => {
                warn!(%err, "Failed to deserialize event envelope, skipping");
                continue;
            }
        };

        let llm_response: LlmResponse = match serde_json::from_value(envelope.payload.clone()) {
            Ok(lr) => lr,
            Err(err) => {
                warn!(%err, "Failed to deserialize LlmResponse payload, skipping");
                continue;
            }
        };

        // Only process if there is text content
        let content = match &llm_response.content {
            Some(c) if !c.is_empty() => c.clone(),
            _ => {
                info!("LLM response has no text content, skipping task detection");
                continue;
            }
        };

        info!(
            agent_id = %llm_response.agent_id,
            model = %llm_response.model_used,
            "Processing LLM response for task detection"
        );

        if let Err(err) = detect_and_publish_tasks(
            &client,
            &http_client,
            &args.anthropic_api_key,
            &args.detection_model,
            &content,
            &llm_response.agent_id,
            &envelope.correlation_id,
            &envelope.scope,
        )
        .await
        {
            error!(
                agent_id = %llm_response.agent_id,
                %err,
                "Failed to detect/publish tasks"
            );
        }
    }

    Ok(())
}

async fn detect_and_publish_tasks(
    nats: &async_nats::Client,
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    llm_content: &str,
    agent_id: &str,
    correlation_id: &Option<String>,
    scope: &Option<String>,
) -> Result<()> {
    // Step 1: Call LLM to detect tasks
    let tasks = detect_tasks_via_llm(http, api_key, model, llm_content).await?;

    if tasks.is_empty() {
        info!("No tasks detected in LLM response");
        return Ok(());
    }

    info!(count = tasks.len(), "Detected tasks in LLM response");

    // Step 2: Publish brain.task.upsert for each detected task
    for task in &tasks {
        let task_scope = task
            .scope
            .clone()
            .or_else(|| scope.clone())
            .unwrap_or_else(|| "scope_root".to_string());

        let upsert_payload = serde_json::json!({
            "title": task.title,
            "description": task.description,
            "status": "open",
            "priority": task.priority,
            "assigned_agent": agent_id,
            "due_date": task.due_date,
            "context": {
                "scope": task_scope,
            },
        });

        let upsert_envelope = EventEnvelope::new(
            "brain.task.upsert",
            PLUGIN_ID,
            correlation_id.clone(),
            Some(task_scope.clone()),
            &upsert_payload,
        )?;

        nats.publish(
            "brain.task.upsert",
            serde_json::to_vec(&upsert_envelope)?.into(),
        )
        .await
        .context("Failed to publish brain.task.upsert")?;

        info!(
            title = %task.title,
            priority = %task.priority,
            scope = %task_scope,
            "Published task upsert"
        );
    }

    Ok(())
}

async fn detect_tasks_via_llm(
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    llm_content: &str,
) -> Result<Vec<DetectedTask>> {
    let prompt = format!(
        r#"Analyze the following text for actionable tasks, to-dos, reminders, or follow-ups.

Look for patterns like:
- "remind me to...", "I need to...", "follow up on..."
- "don't forget to...", "make sure to...", "we should..."
- "TODO:", "action item:", "next step:"
- Commitments made ("I will...", "I'll...", "let me...")
- Requests for future action ("can you...", "please...")
- Deadlines or time-sensitive items

Return a JSON array of detected tasks. Each task has:
- "title": a short, actionable title (imperative form, max 80 chars)
- "description": a longer description with context from the text, or null if the title is sufficient
- "priority": one of "low", "medium", "high", "urgent" based on urgency cues in the text
- "scope": the life/work area this belongs to (e.g. "scope_career", "scope_personal", "scope_projects"), or null if unclear
- "due_date": ISO 8601 date string if a deadline is mentioned, or null

Rules:
- Only extract genuine actionable items, not observations or general statements
- Each task should be independently actionable
- Default priority is "medium" unless urgency cues suggest otherwise
- Return ONLY a valid JSON array, no other text. If no tasks found, return []

Text to analyze:
{llm_content}"#,
    );

    let request_body = AnthropicRequest {
        model: model.to_string(),
        max_tokens: 4096,
        messages: vec![AnthropicMessage {
            role: "user".to_string(),
            content: prompt,
        }],
    };

    let response = http
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .timeout(Duration::from_secs(60))
        .json(&request_body)
        .send()
        .await
        .context("Anthropic API request failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Anthropic API returned {}: {}", status, body);
    }

    let api_response: AnthropicResponse = response
        .json()
        .await
        .context("Failed to parse Anthropic API response")?;

    let text_content = api_response
        .content
        .iter()
        .find_map(|c| c.text.as_ref())
        .context("No text content in Anthropic response")?;

    // Parse the JSON from the LLM response -- strip any markdown fencing if present
    let json_str = text_content
        .trim()
        .strip_prefix("```json")
        .or_else(|| text_content.trim().strip_prefix("```"))
        .unwrap_or(text_content.trim());
    let json_str = json_str
        .strip_suffix("```")
        .unwrap_or(json_str)
        .trim();

    let tasks: Vec<DetectedTask> =
        serde_json::from_str(json_str).context("Failed to parse LLM task detection output")?;

    // Validate tasks
    let valid_tasks: Vec<DetectedTask> = tasks
        .into_iter()
        .filter(|t| {
            if t.title.is_empty() {
                warn!("Dropping task with empty title");
                return false;
            }
            let valid_priorities = ["low", "medium", "high", "urgent"];
            if !valid_priorities.contains(&t.priority.as_str()) {
                warn!(
                    title = %t.title,
                    priority = %t.priority,
                    "Dropping task with invalid priority"
                );
                return false;
            }
            true
        })
        .collect();

    Ok(valid_tasks)
}
