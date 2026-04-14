use anyhow::{Context, Result};
use clap::Parser;
use seidrum_common::events::{
    AgentWake, EventEnvelope, FactUpsertRequest, LlmResponse, PluginRegister,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{error, info, warn};

const PLUGIN_ID: &str = "event-emitter";
const PLUGIN_NAME: &str = "Event Emitter";
const PLUGIN_VERSION: &str = "0.1.0";

#[derive(Parser, Debug)]
#[command(name = "seidrum-event-emitter")]
#[command(about = "Parses LLM responses for structured actions and emits events")]
struct Args {
    /// Bus server URL
    #[arg(long, env = "BUS_URL", default_value = "ws://127.0.0.1:9000")]
    bus_url: String,
}

/// A task block extracted from LLM response.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct TaskBlock {
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "default_priority")]
    priority: String,
    #[serde(default)]
    assigned_agent: Option<String>,
    #[serde(default)]
    due_date: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

fn default_priority() -> String {
    "medium".to_string()
}

/// A fact block extracted from LLM response.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct FactBlock {
    subject: String,
    predicate: String,
    #[serde(default)]
    object: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default = "default_confidence")]
    confidence: f64,
}

fn default_confidence() -> f64 {
    0.8
}

/// A wake block extracted from LLM response.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct WakeBlock {
    agent_id: String,
    reason: String,
    #[serde(default)]
    context: HashMap<String, String>,
}

/// Container for all structured blocks found in LLM response.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
struct StructuredActions {
    #[serde(default)]
    tasks: Vec<TaskBlock>,
    #[serde(default)]
    facts: Vec<FactBlock>,
    #[serde(default)]
    wake: Vec<WakeBlock>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    info!(plugin = PLUGIN_ID, "Starting event emitter plugin");

    // Connect to NATS
    let client = seidrum_common::bus_client::BusClient::connect(&args.bus_url, "event-emitter")
        .await
        .context("Failed to connect to NATS")?;

    info!(url = %args.bus_url, "Connected to NATS");

    // Register plugin
    let register = PluginRegister {
        id: PLUGIN_ID.to_string(),
        name: PLUGIN_NAME.to_string(),
        version: PLUGIN_VERSION.to_string(),
        description: "Parses LLM responses for structured actions and emits events".to_string(),
        consumes: vec!["llm.response".to_string()],
        produces: vec![
            "brain.task.upsert".to_string(),
            "brain.fact.upsert".to_string(),
            "agent.*.wake".to_string(),
        ],
        health_subject: format!("plugin.{}.health", PLUGIN_ID),
        consumed_event_types: vec![],
        produced_event_types: vec![],
        config_schema: None,
    };

    let register_envelope =
        EventEnvelope::new("plugin.register", PLUGIN_ID, None, None, &register)?;

    client
        .publish_bytes("plugin.register", serde_json::to_vec(&register_envelope)?)
        .await
        .context("Failed to publish plugin.register")?;

    info!("Plugin registered");

    // Subscribe to llm.response
    let mut subscriber = client
        .subscribe("llm.response")
        .await
        .context("Failed to subscribe to llm.response")?;

    info!("Subscribed to llm.response, waiting for events...");

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

        info!(
            agent_id = %llm_response.agent_id,
            model = %llm_response.model_used,
            "Processing LLM response for structured actions"
        );

        if let Err(err) = process_response(
            &client,
            &llm_response,
            &envelope.correlation_id,
            &envelope.scope,
        )
        .await
        {
            error!(
                agent_id = %llm_response.agent_id,
                %err,
                "Failed to process LLM response for events"
            );
        }
    }

    Ok(())
}

/// Extract structured action blocks from LLM response content.
///
/// Looks for JSON blocks delimited by markers like:
///   ```json:tasks [...]```
///   ```json:facts [...]```
///   ```json:wake [...]```
///
/// Or a single unified block:
///   ```json:actions {"tasks": [...], "facts": [...], "wake": [...]}```
///
/// Also parses standalone JSON objects/arrays between triple-backtick fences.
fn extract_actions(content: &str) -> StructuredActions {
    let mut actions = StructuredActions::default();

    // Strategy 1: Look for typed markers like ```json:tasks, ```json:facts, ```json:wake
    for marker_type in &["tasks", "facts", "wake"] {
        let marker = format!("```json:{}", marker_type);
        if let Some(start_idx) = content.find(&marker) {
            let after_marker = &content[start_idx + marker.len()..];
            if let Some(end_idx) = after_marker.find("```") {
                let json_str = after_marker[..end_idx].trim();
                match *marker_type {
                    "tasks" => {
                        if let Ok(tasks) = serde_json::from_str::<Vec<TaskBlock>>(json_str) {
                            actions.tasks.extend(tasks);
                        }
                    }
                    "facts" => {
                        if let Ok(facts) = serde_json::from_str::<Vec<FactBlock>>(json_str) {
                            actions.facts.extend(facts);
                        }
                    }
                    "wake" => {
                        if let Ok(wakes) = serde_json::from_str::<Vec<WakeBlock>>(json_str) {
                            actions.wake.extend(wakes);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Strategy 2: Look for ```json:actions unified block
    let actions_marker = "```json:actions";
    if let Some(start_idx) = content.find(actions_marker) {
        let after_marker = &content[start_idx + actions_marker.len()..];
        if let Some(end_idx) = after_marker.find("```") {
            let json_str = after_marker[..end_idx].trim();
            if let Ok(unified) = serde_json::from_str::<StructuredActions>(json_str) {
                actions.tasks.extend(unified.tasks);
                actions.facts.extend(unified.facts);
                actions.wake.extend(unified.wake);
            }
        }
    }

    // Strategy 3: If nothing found yet, try to find any JSON block that looks like actions
    if actions.tasks.is_empty() && actions.facts.is_empty() && actions.wake.is_empty() {
        // Look for generic ```json blocks
        let mut search_from = 0;
        while let Some(start) = content[search_from..].find("```json") {
            let abs_start = search_from + start;
            let after_fence = &content[abs_start + 7..]; // skip "```json"
                                                         // Skip the marker suffix (e.g., ":tasks") if present — already handled above
            let json_start = after_fence.find('\n').map(|i| i + 1).unwrap_or(0);
            if let Some(end) = after_fence[json_start..].find("```") {
                let json_str = after_fence[json_start..json_start + end].trim();
                // Try to parse as unified actions object
                if let Ok(unified) = serde_json::from_str::<StructuredActions>(json_str) {
                    actions.tasks.extend(unified.tasks);
                    actions.facts.extend(unified.facts);
                    actions.wake.extend(unified.wake);
                }
                search_from = abs_start + 7 + json_start + end + 3;
            } else {
                break;
            }
        }
    }

    actions
}

async fn process_response(
    nats: &seidrum_common::bus_client::BusClient,
    llm_response: &LlmResponse,
    correlation_id: &Option<String>,
    scope: &Option<String>,
) -> Result<()> {
    let content = match &llm_response.content {
        Some(c) => c,
        None => {
            info!(
                agent_id = %llm_response.agent_id,
                "LLM response has no content, skipping"
            );
            return Ok(());
        }
    };

    let actions = extract_actions(content);

    let total = actions.tasks.len() + actions.facts.len() + actions.wake.len();
    if total == 0 {
        info!(
            agent_id = %llm_response.agent_id,
            "No structured actions found in LLM response"
        );
        return Ok(());
    }

    info!(
        agent_id = %llm_response.agent_id,
        tasks = actions.tasks.len(),
        facts = actions.facts.len(),
        wake = actions.wake.len(),
        "Extracted structured actions from LLM response"
    );

    // Emit task events via brain.task.upsert
    for task in &actions.tasks {
        let task_payload = serde_json::json!({
            "title": task.title,
            "description": task.description,
            "priority": task.priority,
            "status": "open",
            "assigned_agent": task.assigned_agent.as_deref().unwrap_or(&llm_response.agent_id),
            "due_date": task.due_date,
            "context": {
                "source_agent": llm_response.agent_id,
                "source_model": llm_response.model_used,
            },
        });

        let task_envelope = EventEnvelope::new(
            "brain.task.upsert",
            PLUGIN_ID,
            correlation_id.clone(),
            scope.clone(),
            &task_payload,
        )?;

        nats.publish_bytes("brain.task.upsert", serde_json::to_vec(&task_envelope)?)
            .await
            .context("Failed to publish brain.task.upsert")?;

        info!(title = %task.title, "Published task upsert");
    }

    // Emit fact events via brain.fact.upsert
    for fact in &actions.facts {
        let fact_upsert = FactUpsertRequest {
            subject: fact.subject.clone(),
            predicate: fact.predicate.clone(),
            object: fact.object.clone(),
            value: fact.value.clone(),
            confidence: fact.confidence,
            source_content: format!("llm-response:{}", llm_response.agent_id),
            valid_from: None,
        };

        let fact_envelope = EventEnvelope::new(
            "brain.fact.upsert",
            PLUGIN_ID,
            correlation_id.clone(),
            scope.clone(),
            &fact_upsert,
        )?;

        nats.publish_bytes("brain.fact.upsert", serde_json::to_vec(&fact_envelope)?)
            .await
            .context("Failed to publish brain.fact.upsert")?;

        info!(
            subject = %fact.subject,
            predicate = %fact.predicate,
            "Published fact upsert"
        );
    }

    // Emit wake events via agent.{id}.wake
    for wake in &actions.wake {
        let agent_wake = AgentWake {
            agent_id: wake.agent_id.clone(),
            reason: wake.reason.clone(),
            context: wake.context.clone(),
        };

        let wake_subject = format!("agent.{}.wake", wake.agent_id);

        let wake_envelope = EventEnvelope::new(
            &wake_subject,
            PLUGIN_ID,
            correlation_id.clone(),
            scope.clone(),
            &agent_wake,
        )?;

        let wake_bytes = serde_json::to_vec(&wake_envelope)?;
        nats.publish_bytes(wake_subject, wake_bytes)
            .await
            .context("Failed to publish agent wake event")?;

        info!(
            agent_id = %wake.agent_id,
            reason = %wake.reason,
            "Published agent wake"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_typed_task_block() {
        let content = r#"Here is my analysis.

```json:tasks
[{"title": "Follow up with Acme", "priority": "high"}]
```

Some more text."#;

        let actions = extract_actions(content);
        assert_eq!(actions.tasks.len(), 1);
        assert_eq!(actions.tasks[0].title, "Follow up with Acme");
        assert_eq!(actions.tasks[0].priority, "high");
    }

    #[test]
    fn test_extract_typed_facts_block() {
        let content = r#"Analysis complete.

```json:facts
[{"subject": "entities/acme", "predicate": "industry", "value": "technology", "confidence": 0.95}]
```
"#;

        let actions = extract_actions(content);
        assert_eq!(actions.facts.len(), 1);
        assert_eq!(actions.facts[0].subject, "entities/acme");
        assert_eq!(actions.facts[0].predicate, "industry");
    }

    #[test]
    fn test_extract_typed_wake_block() {
        let content = r#"Need to escalate.

```json:wake
[{"agent_id": "research-agent", "reason": "Need deeper analysis"}]
```
"#;

        let actions = extract_actions(content);
        assert_eq!(actions.wake.len(), 1);
        assert_eq!(actions.wake[0].agent_id, "research-agent");
    }

    #[test]
    fn test_extract_unified_actions_block() {
        let content = r#"Here are the results.

```json:actions
{
  "tasks": [{"title": "Review PR", "priority": "medium"}],
  "facts": [{"subject": "entities/pr-123", "predicate": "status", "value": "open", "confidence": 0.9}],
  "wake": []
}
```
"#;

        let actions = extract_actions(content);
        assert_eq!(actions.tasks.len(), 1);
        assert_eq!(actions.facts.len(), 1);
        assert!(actions.wake.is_empty());
    }

    #[test]
    fn test_extract_no_actions() {
        let content = "Just a plain text response with no structured blocks.";
        let actions = extract_actions(content);
        assert!(actions.tasks.is_empty());
        assert!(actions.facts.is_empty());
        assert!(actions.wake.is_empty());
    }

    #[test]
    fn test_extract_generic_json_block_with_actions_shape() {
        let content = r#"Here is the result:

```json
{
  "tasks": [{"title": "Send email", "priority": "low"}],
  "facts": [],
  "wake": []
}
```
"#;

        let actions = extract_actions(content);
        assert_eq!(actions.tasks.len(), 1);
        assert_eq!(actions.tasks[0].title, "Send email");
    }

    #[test]
    fn test_multiple_typed_blocks() {
        let content = r#"Analysis:

```json:tasks
[{"title": "Task A"}]
```

```json:facts
[{"subject": "e/1", "predicate": "is", "value": "good", "confidence": 0.8}]
```

```json:wake
[{"agent_id": "helper", "reason": "assist"}]
```
"#;

        let actions = extract_actions(content);
        assert_eq!(actions.tasks.len(), 1);
        assert_eq!(actions.facts.len(), 1);
        assert_eq!(actions.wake.len(), 1);
    }
}
