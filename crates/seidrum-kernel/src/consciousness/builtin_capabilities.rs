//! Built-in capability handlers for consciousness agents.
//!
//! These capabilities are always available to every agent and are handled
//! directly by the consciousness service via NATS request/reply on
//! `capability.call.consciousness`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use futures::StreamExt;
use seidrum_common::events::{
    ChannelOutbound, ConsciousnessEvent, ConversationAppendRequest, ConversationAppendResponse,
    ConversationCreateRequest, ConversationCreateResponse, ConversationGetRequest,
    ConversationListRequest, ConversationListResponse, ConversationMessage, DelegateTaskRequest,
    EventEnvelope, ScheduleWakeRequest, SendNotificationRequest, SubscribeEventsRequest,
    ToolCallResponse, UnsubscribeEventsRequest,
};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

/// A runtime event subscription for an agent.
pub struct RuntimeSubscription {
    pub pattern: String,
    pub expiry: Option<chrono::DateTime<Utc>>,
    pub handle: JoinHandle<()>,
}

/// Handle `subscribe-events` — dynamically subscribe an agent to NATS subjects.
pub async fn handle_subscribe_events(
    nats: &async_nats::Client,
    agent_id: &str,
    args: &SubscribeEventsRequest,
    runtime_subs: &Arc<RwLock<HashMap<String, Vec<RuntimeSubscription>>>>,
) -> ToolCallResponse {
    let mut new_subs = Vec::new();
    let expiry = args
        .duration_seconds
        .map(|d| Utc::now() + chrono::Duration::seconds(d as i64));

    for subject in &args.subjects {
        let nats_clone = nats.clone();
        let agent_id_clone = agent_id.to_string();
        let subject_clone = subject.clone();

        let handle = tokio::spawn(async move {
            let mut sub = match nats_clone.subscribe(subject_clone.clone()).await {
                Ok(s) => s,
                Err(e) => {
                    error!(%e, %subject_clone, "Failed to subscribe");
                    return;
                }
            };

            loop {
                // Check expiry before each message
                if let Some(exp) = expiry {
                    if Utc::now() > exp {
                        info!(%subject_clone, "Runtime subscription expired");
                        break;
                    }
                }

                let msg = match tokio::time::timeout(Duration::from_secs(30), sub.next()).await {
                    Ok(Some(msg)) => msg,
                    Ok(None) => break,  // subscription closed
                    Err(_) => continue, // timeout — loop back to check expiry
                };

                let payload: serde_json::Value =
                    serde_json::from_slice(&msg.payload).unwrap_or(serde_json::json!(null));

                let event = ConsciousnessEvent {
                    agent_id: agent_id_clone.clone(),
                    event_type: "subscribed_event".to_string(),
                    source_subject: Some(msg.subject.to_string()),
                    conversation_id: None,
                    payload,
                    origin: None,
                };

                let consciousness_subject = format!("agent.{}.consciousness", agent_id_clone);
                if let Ok(bytes) = serde_json::to_vec(&event) {
                    if let Err(e) = nats_clone
                        .publish(consciousness_subject, bytes.into())
                        .await
                    {
                        warn!(%e, "Failed to publish consciousness event");
                    }
                }
            }
        });

        new_subs.push(RuntimeSubscription {
            pattern: subject.clone(),
            expiry,
            handle,
        });
    }

    let mut subs = runtime_subs.write().await;
    let agent_subs = subs.entry(agent_id.to_string()).or_default();
    let count = new_subs.len();
    agent_subs.extend(new_subs);

    info!(agent_id, count, "Runtime subscriptions added");

    ToolCallResponse {
        tool_id: "subscribe-events".to_string(),
        result: serde_json::json!({
            "subscribed": args.subjects,
            "count": count,
            "expires_at": expiry.map(|e| e.to_rfc3339()),
        }),
        is_error: false,
    }
}

/// Handle `unsubscribe-events` — remove runtime subscriptions.
pub async fn handle_unsubscribe_events(
    agent_id: &str,
    args: &UnsubscribeEventsRequest,
    runtime_subs: &Arc<RwLock<HashMap<String, Vec<RuntimeSubscription>>>>,
) -> ToolCallResponse {
    let mut subs = runtime_subs.write().await;
    let mut removed = 0;

    if let Some(agent_subs) = subs.get_mut(agent_id) {
        agent_subs.retain(|s| {
            if args.subjects.contains(&s.pattern) {
                s.handle.abort();
                removed += 1;
                false
            } else {
                true
            }
        });
    }

    info!(agent_id, removed, "Runtime subscriptions removed");

    ToolCallResponse {
        tool_id: "unsubscribe-events".to_string(),
        result: serde_json::json!({ "removed": removed }),
        is_error: false,
    }
}

/// Handle `delegate-task` — create an agent-to-agent conversation.
pub async fn handle_delegate_task(
    nats: &async_nats::Client,
    from_agent_id: &str,
    args: &DelegateTaskRequest,
) -> ToolCallResponse {
    // Create a conversation between the two agents
    let create_req = ConversationCreateRequest {
        platform: "agent".to_string(),
        participants: vec![
            format!("agent:{}", from_agent_id),
            format!("agent:{}", args.to_agent_id),
        ],
        agent_id: args.to_agent_id.clone(),
        scope: "scope_root".to_string(),
        metadata: HashMap::from([
            ("from_agent".to_string(), from_agent_id.to_string()),
            ("to_agent".to_string(), args.to_agent_id.clone()),
        ]),
    };

    let conv_id = match tokio::time::timeout(
        Duration::from_secs(5),
        nats_request::<_, ConversationCreateResponse>(
            nats,
            "brain.conversation.create",
            &create_req,
        ),
    )
    .await
    {
        Ok(Ok(resp)) => resp.conversation_id,
        Ok(Err(e)) => {
            return ToolCallResponse {
                tool_id: "delegate-task".to_string(),
                result: serde_json::json!({"error": format!("Failed to create conversation: {}", e)}),
                is_error: true,
            };
        }
        Err(_) => {
            return ToolCallResponse {
                tool_id: "delegate-task".to_string(),
                result: serde_json::json!({"error": "Timeout creating conversation"}),
                is_error: true,
            };
        }
    };

    // Append the delegation message
    let append_req = ConversationAppendRequest {
        conversation_id: conv_id.clone(),
        message: ConversationMessage {
            role: "user".to_string(),
            content: Some(args.message.clone()),
            tool_calls: vec![],
            tool_results: vec![],
            media: vec![],
            timestamp: Utc::now(),
            active_skills: vec![],
        },
    };

    let _ = nats_request::<_, ConversationAppendResponse>(
        nats,
        "brain.conversation.append",
        &append_req,
    )
    .await;

    // Send consciousness event to the target agent
    let event = ConsciousnessEvent {
        agent_id: args.to_agent_id.clone(),
        event_type: "agent_message".to_string(),
        source_subject: None,
        conversation_id: Some(conv_id.clone()),
        payload: serde_json::json!({
            "from_agent": from_agent_id,
            "message": args.message,
            "context": args.context,
        }),
        origin: None,
    };

    let subject = format!("agent.{}.consciousness", args.to_agent_id);
    if let Ok(bytes) = serde_json::to_vec(&event) {
        let _ = nats.publish(subject, bytes.into()).await;
    }

    info!(
        from = from_agent_id,
        to = %args.to_agent_id,
        %conv_id,
        "Task delegated"
    );

    ToolCallResponse {
        tool_id: "delegate-task".to_string(),
        result: serde_json::json!({
            "delegated_to": args.to_agent_id,
            "conversation_id": conv_id,
        }),
        is_error: false,
    }
}

/// Handle `schedule-wake` — set a timer that fires on the consciousness stream.
pub async fn handle_schedule_wake(
    nats: &async_nats::Client,
    agent_id: &str,
    args: &ScheduleWakeRequest,
) -> ToolCallResponse {
    let nats_clone = nats.clone();
    let agent_id_clone = agent_id.to_string();
    let reason = args.reason.clone();
    let context = args.context.clone();
    let delay = args.delay_seconds;
    let scheduled_at = Utc::now();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(delay)).await;

        let event = ConsciousnessEvent {
            agent_id: agent_id_clone.clone(),
            event_type: "self_wake".to_string(),
            source_subject: None,
            conversation_id: None,
            payload: serde_json::json!({
                "reason": reason,
                "context": context,
                "scheduled_at": scheduled_at.to_rfc3339(),
                "fired_at": Utc::now().to_rfc3339(),
            }),
            origin: None,
        };

        let subject = format!("agent.{}.consciousness", agent_id_clone);
        if let Ok(bytes) = serde_json::to_vec(&event) {
            let _ = nats_clone.publish(subject, bytes.into()).await;
        }

        info!(
            agent_id = %agent_id_clone,
            %reason,
            "Self-wake timer fired"
        );
    });

    let fire_at = Utc::now() + chrono::Duration::seconds(delay as i64);

    ToolCallResponse {
        tool_id: "schedule-wake".to_string(),
        result: serde_json::json!({
            "scheduled": true,
            "delay_seconds": delay,
            "fire_at": fire_at.to_rfc3339(),
            "reason": args.reason,
        }),
        is_error: false,
    }
}

/// Handle `send-notification` — proactively message the user.
pub async fn handle_send_notification(
    nats: &async_nats::Client,
    args: &SendNotificationRequest,
) -> ToolCallResponse {
    let outbound = ChannelOutbound {
        platform: args.channel.clone(),
        chat_id: args.chat_id.clone(),
        text: args.text.clone(),
        format: "plain".to_string(),
        reply_to: None,
        actions: vec![],
    };

    let subject = format!("channel.{}.outbound", args.channel);
    let envelope = match EventEnvelope::new(&subject, "consciousness", None, None, &outbound) {
        Ok(e) => e,
        Err(e) => {
            return ToolCallResponse {
                tool_id: "send-notification".to_string(),
                result: serde_json::json!({"error": format!("Failed to create envelope: {}", e)}),
                is_error: true,
            };
        }
    };

    let bytes = match serde_json::to_vec(&envelope) {
        Ok(b) => b,
        Err(e) => {
            return ToolCallResponse {
                tool_id: "send-notification".to_string(),
                result: serde_json::json!({"error": format!("Serialization failed: {}", e)}),
                is_error: true,
            };
        }
    };

    match nats.publish(subject, bytes.into()).await {
        Ok(_) => ToolCallResponse {
            tool_id: "send-notification".to_string(),
            result: serde_json::json!({
                "sent": true,
                "channel": args.channel,
                "chat_id": args.chat_id,
            }),
            is_error: false,
        },
        Err(e) => ToolCallResponse {
            tool_id: "send-notification".to_string(),
            result: serde_json::json!({"error": format!("Failed to publish: {}", e)}),
            is_error: true,
        },
    }
}

/// Handle `get-conversation` — load a conversation by ID.
pub async fn handle_get_conversation(
    nats: &async_nats::Client,
    args: &ConversationGetRequest,
) -> ToolCallResponse {
    match tokio::time::timeout(
        Duration::from_secs(5),
        nats_request::<_, serde_json::Value>(nats, "brain.conversation.get", args),
    )
    .await
    {
        Ok(Ok(conv)) => ToolCallResponse {
            tool_id: "get-conversation".to_string(),
            result: conv,
            is_error: false,
        },
        Ok(Err(e)) => ToolCallResponse {
            tool_id: "get-conversation".to_string(),
            result: serde_json::json!({"error": e.to_string()}),
            is_error: true,
        },
        Err(_) => ToolCallResponse {
            tool_id: "get-conversation".to_string(),
            result: serde_json::json!({"error": "timeout"}),
            is_error: true,
        },
    }
}

/// Handle `list-conversations` — list recent conversations.
pub async fn handle_list_conversations(
    nats: &async_nats::Client,
    agent_id: &str,
    args: &ConversationListRequest,
) -> ToolCallResponse {
    let mut req = args.clone();
    req.agent_id = agent_id.to_string();

    match tokio::time::timeout(
        Duration::from_secs(5),
        nats_request::<_, ConversationListResponse>(nats, "brain.conversation.list", &req),
    )
    .await
    {
        Ok(Ok(resp)) => ToolCallResponse {
            tool_id: "list-conversations".to_string(),
            result: serde_json::to_value(resp).unwrap_or(serde_json::json!(null)),
            is_error: false,
        },
        Ok(Err(e)) => ToolCallResponse {
            tool_id: "list-conversations".to_string(),
            result: serde_json::json!({"error": e.to_string()}),
            is_error: true,
        },
        Err(_) => ToolCallResponse {
            tool_id: "list-conversations".to_string(),
            result: serde_json::json!({"error": "timeout"}),
            is_error: true,
        },
    }
}

// ---------------------------------------------------------------------------
// Skill handlers
// ---------------------------------------------------------------------------

/// Handle `search-skills` — search skills by text query via brain service.
pub async fn handle_search_skills(
    nats: &async_nats::Client,
    args: &seidrum_common::events::SkillSearchRequest,
) -> ToolCallResponse {
    match tokio::time::timeout(
        Duration::from_secs(5),
        nats_request::<_, seidrum_common::events::SkillSearchResponse>(
            nats,
            "brain.skill.search",
            args,
        ),
    )
    .await
    {
        Ok(Ok(resp)) => ToolCallResponse {
            tool_id: "search-skills".to_string(),
            result: serde_json::to_value(resp).unwrap_or(serde_json::json!(null)),
            is_error: false,
        },
        Ok(Err(e)) => ToolCallResponse {
            tool_id: "search-skills".to_string(),
            result: serde_json::json!({"error": e.to_string()}),
            is_error: true,
        },
        Err(_) => ToolCallResponse {
            tool_id: "search-skills".to_string(),
            result: serde_json::json!({"error": "timeout"}),
            is_error: true,
        },
    }
}

/// Handle `load-skill` — load a specific skill by ID.
pub async fn handle_load_skill(
    nats: &async_nats::Client,
    args: &seidrum_common::events::SkillGetRequest,
) -> ToolCallResponse {
    match tokio::time::timeout(
        Duration::from_secs(5),
        nats_request::<_, serde_json::Value>(nats, "brain.skill.get", args),
    )
    .await
    {
        Ok(Ok(resp)) => ToolCallResponse {
            tool_id: "load-skill".to_string(),
            result: resp,
            is_error: false,
        },
        Ok(Err(e)) => ToolCallResponse {
            tool_id: "load-skill".to_string(),
            result: serde_json::json!({"error": e.to_string()}),
            is_error: true,
        },
        Err(_) => ToolCallResponse {
            tool_id: "load-skill".to_string(),
            result: serde_json::json!({"error": "timeout"}),
            is_error: true,
        },
    }
}

/// Handle `save-skill` — save a learned behavioral skill.
pub async fn handle_save_skill(
    nats: &async_nats::Client,
    args: &seidrum_common::events::SkillSaveRequest,
) -> ToolCallResponse {
    match tokio::time::timeout(
        Duration::from_secs(5),
        nats_request::<_, seidrum_common::events::SkillSaveResponse>(
            nats,
            "brain.skill.save",
            args,
        ),
    )
    .await
    {
        Ok(Ok(resp)) => ToolCallResponse {
            tool_id: "save-skill".to_string(),
            result: serde_json::to_value(resp).unwrap_or(serde_json::json!(null)),
            is_error: false,
        },
        Ok(Err(e)) => ToolCallResponse {
            tool_id: "save-skill".to_string(),
            result: serde_json::json!({"error": e.to_string()}),
            is_error: true,
        },
        Err(_) => ToolCallResponse {
            tool_id: "save-skill".to_string(),
            result: serde_json::json!({"error": "timeout"}),
            is_error: true,
        },
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// NATS request/reply helper.
async fn nats_request<T: serde::Serialize, R: serde::de::DeserializeOwned>(
    nats: &async_nats::Client,
    subject: &str,
    payload: &T,
) -> Result<R> {
    let bytes = serde_json::to_vec(payload)?;
    let response = nats.request(subject.to_string(), bytes.into()).await?;
    let result: R = serde_json::from_slice(&response.payload)?;
    Ok(result)
}

/// Tool schemas for all built-in capabilities (for LLM context).
pub fn builtin_tool_schemas() -> Vec<seidrum_common::events::ToolSchema> {
    use seidrum_common::events::ToolSchema;

    vec![
        ToolSchema {
            name: "brain-query".to_string(),
            description: "Query the knowledge graph using AQL".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query_type": { "type": "string", "enum": ["aql", "get_facts", "get_context"] },
                    "aql": { "type": "string", "description": "AQL query string" },
                    "bind_vars": { "type": "object" },
                    "limit": { "type": "integer" }
                },
                "required": ["query_type"]
            }),
        },
        ToolSchema {
            name: "subscribe-events".to_string(),
            description:
                "Subscribe to NATS event patterns to receive them on your consciousness stream"
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "subjects": { "type": "array", "items": { "type": "string" }, "description": "NATS subject patterns to subscribe to" },
                    "duration_seconds": { "type": "integer", "description": "Optional auto-expiry in seconds" }
                },
                "required": ["subjects"]
            }),
        },
        ToolSchema {
            name: "unsubscribe-events".to_string(),
            description: "Stop watching for events on specific NATS patterns".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "subjects": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["subjects"]
            }),
        },
        ToolSchema {
            name: "delegate-task".to_string(),
            description:
                "Delegate a task to another agent. Creates an agent-to-agent conversation."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "to_agent_id": { "type": "string", "description": "Target agent ID" },
                    "message": { "type": "string", "description": "Task description for the target agent" },
                    "context": { "type": "object", "description": "Additional context" }
                },
                "required": ["to_agent_id", "message"]
            }),
        },
        ToolSchema {
            name: "schedule-wake".to_string(),
            description:
                "Schedule a self-wake timer. You will receive a consciousness event when it fires."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "delay_seconds": { "type": "integer", "description": "Seconds until wake-up" },
                    "reason": { "type": "string", "description": "Why you are scheduling this" },
                    "context": { "type": "object", "description": "Context to include when waking up" }
                },
                "required": ["delay_seconds", "reason"]
            }),
        },
        ToolSchema {
            name: "send-notification".to_string(),
            description: "Send a proactive message to the user on a specific channel".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "channel": { "type": "string", "description": "Channel platform (e.g., telegram, cli)" },
                    "chat_id": { "type": "string", "description": "Chat/conversation ID on the platform" },
                    "text": { "type": "string", "description": "Message text" }
                },
                "required": ["channel", "chat_id", "text"]
            }),
        },
        ToolSchema {
            name: "get-conversation".to_string(),
            description: "Load a conversation thread by ID".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "conversation_id": { "type": "string" },
                    "max_messages": { "type": "integer", "description": "Maximum recent messages to return (0 = all)" }
                },
                "required": ["conversation_id"]
            }),
        },
        ToolSchema {
            name: "list-conversations".to_string(),
            description: "List your recent conversations".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform": { "type": "string", "description": "Filter by platform" },
                    "limit": { "type": "integer", "description": "Max results (default 20)" }
                }
            }),
        },
        ToolSchema {
            name: "search-skills".to_string(),
            description: "Search for behavioral skills by semantic query".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural language query to find relevant skills" },
                    "limit": { "type": "integer", "description": "Max results (default 5)" }
                },
                "required": ["query"]
            }),
        },
        ToolSchema {
            name: "load-skill".to_string(),
            description: "Load a specific skill by ID into the current conversation context"
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "skill_id": { "type": "string", "description": "Skill ID to load" }
                },
                "required": ["skill_id"]
            }),
        },
        ToolSchema {
            name: "save-skill".to_string(),
            description: "Save learned behavior as a reusable skill for future use".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "description": { "type": "string", "description": "What triggers this skill (used for semantic search)" },
                    "snippet": { "type": "string", "description": "The behavioral instruction to inject when this skill is active" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Tags for categorization" }
                },
                "required": ["description", "snippet"]
            }),
        },
    ]
}
