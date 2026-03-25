//! Consciousness service: the core agent runtime.
//!
//! For each loaded agent with `subscribe` entries, creates NATS subscriptions
//! that forward matching events to `agent.{id}.consciousness`. Then processes
//! consciousness events by loading context, calling the LLM, and handling
//! built-in capabilities.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures::StreamExt;
use seidrum_common::config::{load_agent_definition, AgentDefinition};
use seidrum_common::events::{
    ConsciousnessEvent, EventEnvelope, ToolCallRequest, ToolCallResponse, ToolRegister,
};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use super::builtin_capabilities::{self, RuntimeSubscription};

/// The consciousness service manages agent event streams and built-in capabilities.
pub struct ConsciousnessService {
    nats: async_nats::Client,
    agents: HashMap<String, AgentDefinition>,
    system_prompt: String,
    runtime_subs: Arc<RwLock<HashMap<String, Vec<RuntimeSubscription>>>>,
}

impl ConsciousnessService {
    /// Create a new consciousness service by loading agents and the system prompt.
    pub fn new(nats: async_nats::Client, agents_dir: &str) -> Result<Self> {
        let agents = load_agents(agents_dir)?;
        let system_prompt = load_system_prompt()?;

        Ok(Self {
            nats,
            agents,
            system_prompt,
            runtime_subs: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Spawn the consciousness service.
    pub async fn spawn(self) -> Result<tokio::task::JoinHandle<()>> {
        // Register built-in capabilities with the capability registry
        self.register_builtin_capabilities().await?;

        // Create event subscriptions for each agent's `subscribe` patterns
        for (agent_id, agent) in &self.agents {
            for pattern in &agent.subscribe {
                let nats = self.nats.clone();
                let agent_id = agent_id.clone();
                let pattern = pattern.clone();

                tokio::spawn(async move {
                    let mut sub = match nats.subscribe(pattern.clone()).await {
                        Ok(s) => s,
                        Err(e) => {
                            error!(
                                agent_id = %agent_id,
                                pattern = %pattern,
                                error = %e,
                                "Failed to subscribe for consciousness"
                            );
                            return;
                        }
                    };

                    info!(
                        agent_id = %agent_id,
                        pattern = %pattern,
                        "Consciousness subscription active"
                    );

                    while let Some(msg) = sub.next().await {
                        let payload: serde_json::Value =
                            serde_json::from_slice(&msg.payload).unwrap_or(serde_json::json!(null));

                        let event = ConsciousnessEvent {
                            agent_id: agent_id.clone(),
                            event_type: "subscribed_event".to_string(),
                            source_subject: Some(msg.subject.to_string()),
                            conversation_id: None,
                            payload,
                            origin: None,
                        };

                        let subject = format!("agent.{}.consciousness", agent_id);
                        if let Ok(bytes) = serde_json::to_vec(&event) {
                            let _ = nats.publish(subject, bytes.into()).await;
                        }
                    }
                });
            }
        }

        // Subscribe to capability.call.consciousness for built-in capability handling
        let mut capability_sub = self
            .nats
            .subscribe("capability.call.consciousness".to_string())
            .await
            .context("subscribe capability.call.consciousness")?;

        let nats = self.nats.clone();
        let runtime_subs = self.runtime_subs.clone();

        let handle = tokio::spawn(async move {
            info!(
                agent_count = self.agents.len(),
                "Consciousness service started"
            );

            while let Some(msg) = capability_sub.next().await {
                let reply = match msg.reply {
                    Some(ref r) => r.clone(),
                    None => continue,
                };

                let request: ToolCallRequest = match serde_json::from_slice(&msg.payload) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(%e, "Failed to parse capability call for consciousness");
                        continue;
                    }
                };

                let response = handle_builtin_call(&nats, &request, &runtime_subs).await;

                if let Ok(bytes) = serde_json::to_vec(&response) {
                    let _ = nats.publish(reply, bytes.into()).await;
                }
            }

            info!("Consciousness service stopped");
        });

        Ok(handle)
    }

    /// Register all built-in capabilities with the kernel's capability registry.
    async fn register_builtin_capabilities(&self) -> Result<()> {
        let builtins = [
            ("brain-query", "Query the knowledge graph using AQL"),
            (
                "subscribe-events",
                "Subscribe to NATS event patterns for consciousness",
            ),
            ("unsubscribe-events", "Stop watching for NATS events"),
            ("delegate-task", "Delegate a task to another agent"),
            (
                "schedule-wake",
                "Schedule a self-wake timer for future processing",
            ),
            ("send-notification", "Send a proactive message to the user"),
            ("get-conversation", "Load a conversation thread by ID"),
            ("list-conversations", "List recent conversations"),
            (
                "search-skills",
                "Search for behavioral skills by semantic query",
            ),
            (
                "load-skill",
                "Load a specific skill into conversation context",
            ),
            ("save-skill", "Save learned behavior as a reusable skill"),
        ];

        let schemas = builtin_capabilities::builtin_tool_schemas();

        for (tool_id, summary) in &builtins {
            let schema = schemas
                .iter()
                .find(|s| s.name == *tool_id)
                .map(|s| s.parameters.clone())
                .unwrap_or(serde_json::json!({}));

            let register = ToolRegister {
                tool_id: tool_id.to_string(),
                plugin_id: "consciousness".to_string(),
                name: tool_id.to_string(),
                summary_md: summary.to_string(),
                manual_md: String::new(),
                parameters: schema,
                call_subject: "capability.call.consciousness".to_string(),
                kind: "tool".to_string(),
            };

            if let Ok(bytes) = serde_json::to_vec(&register) {
                let _ = self
                    .nats
                    .publish("capability.register".to_string(), bytes.into())
                    .await;
            }
        }

        info!("Registered {} built-in capabilities", builtins.len());
        Ok(())
    }
}

/// Helper to parse tool call arguments, returning an error response on failure.
fn parse_args<T: serde::de::DeserializeOwned>(
    tool_id: &str,
    arguments: &serde_json::Value,
) -> Result<T, ToolCallResponse> {
    serde_json::from_value(arguments.clone()).map_err(|e| ToolCallResponse {
        tool_id: tool_id.to_string(),
        result: serde_json::json!({"error": format!("Invalid arguments: {}", e)}),
        is_error: true,
    })
}

/// Handle a built-in capability call by routing to the appropriate handler.
async fn handle_builtin_call(
    nats: &async_nats::Client,
    request: &ToolCallRequest,
    runtime_subs: &Arc<RwLock<HashMap<String, Vec<RuntimeSubscription>>>>,
) -> ToolCallResponse {
    // Extract agent_id from plugin_id field (set by the tool dispatcher)
    let agent_id = &request.plugin_id;

    match request.tool_id.as_str() {
        "brain-query" => {
            // Wrap arguments in an EventEnvelope as the brain service expects
            let envelope = match EventEnvelope::new(
                "brain.query.request",
                "consciousness",
                request.correlation_id.clone(),
                None,
                &request.arguments,
            ) {
                Ok(e) => e,
                Err(e) => {
                    return ToolCallResponse {
                        tool_id: "brain-query".to_string(),
                        result: serde_json::json!({"error": format!("Failed to create envelope: {}", e)}),
                        is_error: true,
                    };
                }
            };

            match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                nats_request::<_, serde_json::Value>(nats, "brain.query.request", &envelope),
            )
            .await
            {
                Ok(Ok(result)) => ToolCallResponse {
                    tool_id: "brain-query".to_string(),
                    result,
                    is_error: false,
                },
                Ok(Err(e)) => ToolCallResponse {
                    tool_id: "brain-query".to_string(),
                    result: serde_json::json!({"error": e.to_string()}),
                    is_error: true,
                },
                Err(_) => ToolCallResponse {
                    tool_id: "brain-query".to_string(),
                    result: serde_json::json!({"error": "brain query timeout"}),
                    is_error: true,
                },
            }
        }

        "subscribe-events" => {
            let args: SubscribeEventsArgs = match parse_args("subscribe-events", &request.arguments)
            {
                Ok(a) => a,
                Err(resp) => return resp,
            };
            let req = seidrum_common::events::SubscribeEventsRequest {
                subjects: args.subjects,
                duration_seconds: args.duration_seconds,
            };
            builtin_capabilities::handle_subscribe_events(nats, agent_id, &req, runtime_subs).await
        }

        "unsubscribe-events" => {
            let args: UnsubscribeEventsArgs =
                match parse_args("unsubscribe-events", &request.arguments) {
                    Ok(a) => a,
                    Err(resp) => return resp,
                };
            let req = seidrum_common::events::UnsubscribeEventsRequest {
                subjects: args.subjects,
            };
            builtin_capabilities::handle_unsubscribe_events(agent_id, &req, runtime_subs).await
        }

        "delegate-task" => {
            let args: DelegateTaskArgs = match parse_args("delegate-task", &request.arguments) {
                Ok(a) => a,
                Err(resp) => return resp,
            };
            let req = seidrum_common::events::DelegateTaskRequest {
                to_agent_id: args.to_agent_id,
                message: args.message,
                context: args.context,
            };
            builtin_capabilities::handle_delegate_task(nats, agent_id, &req).await
        }

        "schedule-wake" => {
            let args: ScheduleWakeArgs = match parse_args("schedule-wake", &request.arguments) {
                Ok(a) => a,
                Err(resp) => return resp,
            };
            // Validate bounds: min 1s, max 7 days
            if args.delay_seconds == 0 || args.delay_seconds > 604800 {
                return ToolCallResponse {
                    tool_id: "schedule-wake".to_string(),
                    result: serde_json::json!({"error": "delay_seconds must be between 1 and 604800 (7 days)"}),
                    is_error: true,
                };
            }
            let req = seidrum_common::events::ScheduleWakeRequest {
                delay_seconds: args.delay_seconds,
                reason: args.reason,
                context: args.context,
            };
            builtin_capabilities::handle_schedule_wake(nats, agent_id, &req).await
        }

        "send-notification" => {
            let args: SendNotificationArgs =
                match parse_args("send-notification", &request.arguments) {
                    Ok(a) => a,
                    Err(resp) => return resp,
                };
            let req = seidrum_common::events::SendNotificationRequest {
                channel: args.channel,
                chat_id: args.chat_id,
                text: args.text,
                conversation_id: None,
            };
            builtin_capabilities::handle_send_notification(nats, &req).await
        }

        "get-conversation" => {
            let args: GetConversationArgs = match parse_args("get-conversation", &request.arguments)
            {
                Ok(a) => a,
                Err(resp) => return resp,
            };
            let req = seidrum_common::events::ConversationGetRequest {
                conversation_id: args.conversation_id,
                max_messages: args.max_messages.unwrap_or(0),
            };
            builtin_capabilities::handle_get_conversation(nats, &req).await
        }

        "list-conversations" => {
            let args: ListConversationsArgs =
                match parse_args("list-conversations", &request.arguments) {
                    Ok(a) => a,
                    Err(resp) => return resp,
                };
            let req = seidrum_common::events::ConversationListRequest {
                agent_id: agent_id.to_string(),
                platform: args.platform,
                limit: args.limit.unwrap_or(20),
            };
            builtin_capabilities::handle_list_conversations(nats, agent_id, &req).await
        }

        "search-skills" => {
            let args: SearchSkillsArgs = match parse_args("search-skills", &request.arguments) {
                Ok(a) => a,
                Err(resp) => return resp,
            };
            let req = seidrum_common::events::SkillSearchRequest {
                query: args.query,
                limit: args.limit,
                scope: None,
            };
            builtin_capabilities::handle_search_skills(nats, &req).await
        }

        "load-skill" => {
            let args: LoadSkillArgs = match parse_args("load-skill", &request.arguments) {
                Ok(a) => a,
                Err(resp) => return resp,
            };
            if args.skill_id.trim().is_empty() {
                return ToolCallResponse {
                    tool_id: "load-skill".to_string(),
                    result: serde_json::json!({"error": "skill_id is required"}),
                    is_error: true,
                };
            }
            let req = seidrum_common::events::SkillGetRequest {
                skill_id: args.skill_id,
            };
            builtin_capabilities::handle_load_skill(nats, &req).await
        }

        "save-skill" => {
            let args: SaveSkillArgs = match parse_args("save-skill", &request.arguments) {
                Ok(a) => a,
                Err(resp) => return resp,
            };
            if args.description.trim().is_empty() || args.snippet.trim().is_empty() {
                return ToolCallResponse {
                    tool_id: "save-skill".to_string(),
                    result: serde_json::json!({"error": "description and snippet are required"}),
                    is_error: true,
                };
            }
            // Try to generate embedding for the skill
            let embed_text = format!("{}\n{}", args.description, args.snippet);
            let embedding = match crate::embedding::service::EmbeddingService::from_env() {
                Ok(svc) if svc.is_available() => svc.embed(&embed_text).await.unwrap_or_default(),
                _ => vec![],
            };

            let req = seidrum_common::events::SkillSaveRequest {
                id: None,
                description: args.description,
                snippet: args.snippet,
                source: "learned".to_string(),
                scope: None,
                tags: args.tags.unwrap_or_default(),
                learned_from: None,
                embedding,
            };
            builtin_capabilities::handle_save_skill(nats, &req).await
        }

        other => ToolCallResponse {
            tool_id: other.to_string(),
            result: serde_json::json!({"error": format!("Unknown built-in capability: {}", other)}),
            is_error: true,
        },
    }
}

// ---------------------------------------------------------------------------
// Argument parsing structs (from LLM tool call arguments)
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize, Default)]
struct SearchSkillsArgs {
    #[serde(default)]
    query: String,
    limit: Option<u32>,
}

#[derive(serde::Deserialize, Default)]
struct LoadSkillArgs {
    #[serde(default)]
    skill_id: String,
}

#[derive(serde::Deserialize, Default)]
struct SaveSkillArgs {
    #[serde(default)]
    description: String,
    #[serde(default)]
    snippet: String,
    tags: Option<Vec<String>>,
}

#[derive(serde::Deserialize, Default)]
struct SubscribeEventsArgs {
    #[serde(default)]
    subjects: Vec<String>,
    duration_seconds: Option<u64>,
}

#[derive(serde::Deserialize, Default)]
struct UnsubscribeEventsArgs {
    #[serde(default)]
    subjects: Vec<String>,
}

#[derive(serde::Deserialize, Default)]
struct DelegateTaskArgs {
    #[serde(default)]
    to_agent_id: String,
    #[serde(default)]
    message: String,
    context: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, Default)]
struct ScheduleWakeArgs {
    #[serde(default)]
    delay_seconds: u64,
    #[serde(default)]
    reason: String,
    context: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, Default)]
struct SendNotificationArgs {
    #[serde(default)]
    channel: String,
    #[serde(default)]
    chat_id: String,
    #[serde(default)]
    text: String,
}

#[derive(serde::Deserialize, Default)]
struct GetConversationArgs {
    #[serde(default)]
    conversation_id: String,
    max_messages: Option<u32>,
}

#[derive(serde::Deserialize, Default)]
struct ListConversationsArgs {
    platform: Option<String>,
    limit: Option<u32>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn load_agents(agents_dir: &str) -> Result<HashMap<String, AgentDefinition>> {
    let mut agents = HashMap::new();
    let dir = Path::new(agents_dir);

    if !dir.is_dir() {
        warn!("Agents directory not found: {}", agents_dir);
        return Ok(agents);
    }

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "yaml" && ext != "yml" {
            continue;
        }

        match load_agent_definition(&path) {
            Ok(def) => {
                info!(
                    agent_id = %def.agent.id,
                    subscriptions = def.agent.subscribe.len(),
                    "Consciousness: loaded agent"
                );
                agents.insert(def.agent.id.clone(), def.agent);
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "Failed to load agent");
            }
        }
    }

    Ok(agents)
}

fn load_system_prompt() -> Result<String> {
    let path = Path::new("prompts/system.md");
    if path.exists() {
        std::fs::read_to_string(path).context("Failed to read prompts/system.md")
    } else {
        warn!("prompts/system.md not found, using empty system prompt");
        Ok(String::new())
    }
}

// nats_request is in super::nats_request (consciousness/mod.rs)
use super::nats_request;
