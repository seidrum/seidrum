//! Consciousness service: the core agent runtime.
//!
//! For each loaded agent with `subscribe` entries, creates NATS subscriptions
//! that forward matching events to `agent.{id}.consciousness`. Then processes
//! consciousness events by loading context, calling the LLM, and handling
//! built-in capabilities.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use futures::StreamExt;
use seidrum_common::config::{load_agent_definition, AgentDefinition};
use seidrum_common::events::{
    AgentFeedback, ChannelOutbound, ConsciousnessEvent, Conversation, ConversationAppendRequest,
    ConversationAppendResponse, ConversationCreateRequest, ConversationCreateResponse,
    ConversationFindRequest, ConversationGetRequest, ConversationMessage, EventEnvelope,
    EventOrigin, FactUpsertRequest, LlmCallConfig, LlmResponse, PreferencesQueryRequest,
    PreferencesQueryResponse, SkillSearchRequest, SkillSearchResponse, ToolCallRequest,
    ToolCallResponse, ToolRegister, UnifiedLlmRequest, UnifiedMessage, UserPreference,
};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use super::builtin_capabilities::{self, builtin_tool_schemas, RuntimeSubscription};
use super::guardrails::{self, GuardrailState};

/// The consciousness service manages agent event streams and built-in capabilities.
pub struct ConsciousnessService {
    nats: async_nats::Client,
    agents: Arc<RwLock<HashMap<String, AgentDefinition>>>,
    system_prompt: String,
    runtime_subs: Arc<RwLock<HashMap<String, Vec<RuntimeSubscription>>>>,
    /// Cached identity prompts: agent_id -> rendered prompt text.
    identity_prompts: Arc<RwLock<HashMap<String, String>>>,
    /// Path to agents directory, used for reloading agents at runtime
    agents_dir: String,
}

impl ConsciousnessService {
    /// Create a new consciousness service by loading agents and the system prompt.
    pub fn new(nats: async_nats::Client, agents_dir: &str) -> Result<Self> {
        let agents = load_agents(agents_dir)?;
        let system_prompt = load_system_prompt()?;

        // S1+S2: Cache identity prompts at startup to avoid blocking file I/O per event
        let mut identity_prompts = HashMap::new();
        for (agent_id, agent_def) in &agents {
            let prompt = load_agent_identity_prompt(&agent_def.prompt);
            if !prompt.is_empty() {
                info!(agent_id = %agent_id, "Cached identity prompt ({} bytes)", prompt.len());
            }
            identity_prompts.insert(agent_id.clone(), prompt);
        }

        Ok(Self {
            agents_dir: agents_dir.to_string(),
            nats,
            agents: Arc::new(RwLock::new(agents)),
            system_prompt,
            identity_prompts: Arc::new(RwLock::new(identity_prompts)),
            runtime_subs: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Spawn the consciousness service.
    pub async fn spawn(self) -> Result<tokio::task::JoinHandle<()>> {
        // Register built-in capabilities with the capability registry
        self.register_builtin_capabilities().await?;

        // Create event subscriptions for each agent's `subscribe` patterns
        let agents_read = self.agents.read().await;
        for (agent_id, agent) in agents_read.iter() {
            for pattern in &agent.subscribe {
                // I1: Skip channel inbound patterns — handled by the workflow engine
                // to prevent duplicate LLM calls.
                if pattern.starts_with("channel.") && pattern.ends_with(".inbound") {
                    warn!(
                        agent_id = %agent_id,
                        pattern = %pattern,
                        "Skipping channel inbound subscription (handled by workflow engine)"
                    );
                    continue;
                }

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
                        // E1: Reject malformed messages instead of forwarding null
                        let payload: serde_json::Value =
                            match serde_json::from_slice::<serde_json::Value>(&msg.payload) {
                                Ok(v) if !v.is_null() => v,
                                Ok(_) => {
                                    warn!(
                                        agent_id = %agent_id,
                                        subject = %msg.subject,
                                        "Skipping null payload from NATS message"
                                    );
                                    continue;
                                }
                                Err(e) => {
                                    warn!(
                                        agent_id = %agent_id,
                                        subject = %msg.subject,
                                        error = %e,
                                        "Skipping malformed NATS message"
                                    );
                                    continue;
                                }
                            };

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
        drop(agents_read);

        // -----------------------------------------------------------------
        // Consciousness event processing loop (one mpsc channel per agent)
        // -----------------------------------------------------------------
        // Each agent gets a dedicated tokio::mpsc channel so that events are
        // processed SEQUENTIALLY — preventing concurrent conversation mutations.

        let agents_read = self.agents.read().await;
        for (agent_id, agent_def) in agents_read.iter() {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<ConsciousnessEvent>(256);

            // Spawn NATS subscriber → mpsc forwarder
            let nats_sub = self.nats.clone();
            let agent_id_sub = agent_id.clone();
            let tx_clone = tx.clone();
            tokio::spawn(async move {
                let subject = format!("agent.{}.consciousness", agent_id_sub);
                let mut sub = match nats_sub.subscribe(subject.clone()).await {
                    Ok(s) => s,
                    Err(e) => {
                        error!(
                            agent_id = %agent_id_sub,
                            error = %e,
                            "Failed to subscribe to consciousness stream"
                        );
                        return;
                    }
                };

                info!(
                    agent_id = %agent_id_sub,
                    subject = %subject,
                    "Consciousness processing subscription active"
                );

                while let Some(msg) = sub.next().await {
                    let event: ConsciousnessEvent = match serde_json::from_slice(&msg.payload) {
                        Ok(e) => e,
                        Err(e) => {
                            warn!(
                                agent_id = %agent_id_sub,
                                error = %e,
                                "Failed to parse consciousness event"
                            );
                            continue;
                        }
                    };
                    if tx_clone.send(event).await.is_err() {
                        warn!(agent_id = %agent_id_sub, "Consciousness channel closed");
                        break;
                    }
                }
            });

            // Spawn sequential processor
            let nats_proc = self.nats.clone();
            let system_prompt = self.system_prompt.clone();
            let agent_def = agent_def.clone();
            let agent_id_proc = agent_id.clone();
            let identity_prompts = self.identity_prompts.clone();

            tokio::spawn(async move {
                info!(agent_id = %agent_id_proc, "Consciousness processor started");

                while let Some(event) = rx.recv().await {
                    let cached_identity = identity_prompts
                        .read()
                        .await
                        .get(&agent_id_proc)
                        .cloned()
                        .unwrap_or_default();

                    if let Err(e) = process_consciousness_event(
                        &nats_proc,
                        &agent_id_proc,
                        &agent_def,
                        &system_prompt,
                        &cached_identity,
                        &event,
                    )
                    .await
                    {
                        error!(
                            agent_id = %agent_id_proc,
                            event_type = %event.event_type,
                            error = %e,
                            "Consciousness event processing failed"
                        );
                    }
                }

                info!(agent_id = %agent_id_proc, "Consciousness processor stopped");
            });
        }
        drop(agents_read);

        // -----------------------------------------------------------------
        // Feedback handler (Phase 2.3): Subscribe to agent.feedback events
        // and store preference facts in the brain
        // -----------------------------------------------------------------
        let nats_feedback = self.nats.clone();
        tokio::spawn(async move {
            match nats_feedback.subscribe("agent.feedback".to_string()).await {
                Ok(mut feedback_sub) => {
                    info!("Consciousness: feedback handler subscription active");
                    while let Some(msg) = feedback_sub.next().await {
                        let feedback: AgentFeedback = match serde_json::from_slice(&msg.payload) {
                            Ok(f) => f,
                            Err(e) => {
                                warn!(error = %e, "Failed to parse agent.feedback event");
                                continue;
                            }
                        };

                        // If preference info is present, store as a fact
                        if let (Some(key), Some(value)) =
                            (feedback.preference_key, feedback.preference_value)
                        {
                            let _ = store_preference_fact(
                                &nats_feedback,
                                &feedback.agent_id,
                                &key,
                                &value,
                                feedback.confidence,
                            )
                            .await;
                        }
                    }
                }
                Err(e) => {
                    error!(error = %e, "Failed to subscribe to agent.feedback");
                }
            }
        });

        // Subscribe to agent reload events
        let mut reload_sub = self
            .nats
            .subscribe("kernel.agents.reload".to_string())
            .await
            .context("subscribe kernel.agents.reload")?;

        // Subscribe to capability.call.consciousness for built-in capability handling
        let mut capability_sub = self
            .nats
            .subscribe("capability.call.consciousness".to_string())
            .await
            .context("subscribe capability.call.consciousness")?;

        let nats = self.nats.clone();
        let runtime_subs = self.runtime_subs.clone();
        let agents = self.agents.clone();
        let identity_prompts = self.identity_prompts.clone();
        let agents_dir = self.agents_dir.clone();

        let handle = tokio::spawn(async move {
            let agent_count = agents.read().await.len();
            info!(agent_count = agent_count, "Consciousness service started");

            loop {
                tokio::select! {
                    Some(_msg) = reload_sub.next() => {
                        // Agent reload request from management API
                        info!("Reloading agent definitions from disk");
                        if let Err(e) = reload_agents_internal(&agents, &identity_prompts, &agents_dir).await {
                            error!(error = %e, "Failed to reload agents");
                        } else {
                            let count = agents.read().await.len();
                            info!(count = count, "Agents reloaded successfully");
                        }
                    }
                    Some(msg) = capability_sub.next() => {
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
                }
            }
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
                // E2: Log capability registration failures instead of silently ignoring
                if let Err(e) = self
                    .nats
                    .publish("capability.register".to_string(), bytes.into())
                    .await
                {
                    warn!(
                        tool_id = %tool_id,
                        error = %e,
                        "Failed to register built-in capability"
                    );
                }
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
                user_id: None,
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
                user_id: None,
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
// Consciousness event processing
// ---------------------------------------------------------------------------

/// Process a single consciousness event for an agent.
///
/// Steps:
///   1. Find or create a conversation
///   2. Load recent conversation history
///   3. Search for relevant skills
///   4. Build system prompt + skill snippets + identity prompt
///   5. Build UnifiedLlmRequest with messages + tool schemas
///   6. Dispatch to LLM provider via NATS request/reply
///   7. Append user message + assistant response to conversation
///   8. If response has origin, publish to outbound channel
///   9. Track guardrails
async fn process_consciousness_event(
    nats: &async_nats::Client,
    agent_id: &str,
    agent_def: &AgentDefinition,
    system_prompt: &str,
    cached_identity: &str,
    event: &ConsciousnessEvent,
) -> Result<()> {
    info!(
        agent_id = %agent_id,
        event_type = %event.event_type,
        conversation_id = ?event.conversation_id,
        "Processing consciousness event"
    );

    // Build user-facing text from event payload
    let user_text = extract_user_text(event);

    // 1. Find or create conversation
    let conversation_id = find_or_create_conversation(nats, agent_id, agent_def, event).await?;

    // 2. Load recent conversation history
    let history = load_conversation_history(nats, &conversation_id).await;

    // 3. Search for relevant skills based on event content
    let skill_snippets = search_relevant_skills(nats, &user_text, &agent_def.scope).await;

    // 3a. Query user preferences (Phase 2.3)
    let preferences = query_user_preferences(nats, agent_id, &conversation_id).await;

    // 4. Build system prompt: base system prompt + cached identity prompt + skill snippets + preferences
    let full_system_prompt = build_full_system_prompt_with_preferences(
        system_prompt,
        agent_def,
        cached_identity,
        &skill_snippets,
        &preferences,
    );

    // 5. Build messages array from history + current event
    let messages = build_messages(&history, &user_text, event);

    // 6. Build tool schemas, filtered by agent's configured tools (M2)
    let agent_tools = &agent_def.tools;
    let mut tools = builtin_tool_schemas();
    if !agent_tools.is_empty() {
        // Keep only tools that are in the agent's configured list, plus always include brain-query
        tools.retain(|t| agent_tools.contains(&t.name) || t.name == "brain-query");
    }

    // M1: Extract correlation_id from event payload if present
    let correlation_id = event
        .payload
        .get("correlation_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Extract user_id for multi-tenant propagation through the LLM pipeline
    let user_id = event
        .payload
        .get("user_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    // Build UnifiedLlmRequest
    let llm_request = UnifiedLlmRequest {
        agent_id: agent_id.to_string(),
        messages,
        system_prompt: Some(full_system_prompt),
        tools,
        config: LlmCallConfig {
            temperature: Some(0.7),
            max_tokens: Some(4096),
            top_p: None,
        },
        routing_strategy: "best-first".to_string(),
        model_preferences: vec!["gemini-2.0-flash".to_string()],
        correlation_id,
        scope: Some(agent_def.scope.clone()),
        user_id,
    };

    // Create guardrail state
    let mut guardrail = GuardrailState::new(build_guardrail_config(agent_def));

    // C1: Use configurable LLM provider instead of hardcoded "google"
    let llm_provider = std::env::var("LLM_PROVIDER").unwrap_or_else(|_| "google".to_string());
    let llm_subject = format!("llm.provider.{}", llm_provider);

    // 7. Dispatch to LLM provider (120s timeout)
    let llm_response: LlmResponse = match tokio::time::timeout(
        Duration::from_secs(120),
        nats_request::<_, LlmResponse>(nats, &llm_subject, &llm_request),
    )
    .await
    {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => {
            error!(
                agent_id = %agent_id,
                error = %e,
                "LLM request failed"
            );
            return Err(e.context("LLM request failed"));
        }
        Err(_) => {
            error!(agent_id = %agent_id, "LLM request timed out (120s)");
            return Err(anyhow::anyhow!("LLM request timed out"));
        }
    };

    // Track guardrail rounds from provider
    if llm_response.tool_rounds > 0 {
        guardrail.add_provider_rounds(llm_response.tool_rounds);
    }

    // G1: Check guardrails after LLM response and enforce violations
    let mut guardrail_triggered = false;
    let mut guardrail_msg = String::new();
    if let Some(tool_calls) = &llm_response.tool_calls {
        for tc in tool_calls {
            if let Some(violation) = guardrail.record_turn(&tc.function_name, &tc.arguments) {
                warn!(
                    agent_id = %agent_id,
                    ?violation,
                    "Guardrail triggered"
                );
                guardrail_msg = match violation {
                    guardrails::GuardrailViolation::TurnLimitReached(n) => {
                        format!("Tool call limit reached ({} turns). Stopping.", n)
                    }
                    guardrails::GuardrailViolation::TimeLimitReached(s) => {
                        format!("Time limit reached ({}s). Stopping.", s)
                    }
                    guardrails::GuardrailViolation::LoopDetected { tool_name, count } => {
                        format!(
                            "Loop detected: {} called {} times with same args.",
                            tool_name, count
                        )
                    }
                    guardrails::GuardrailViolation::HitlRequired(n) => {
                        format!("Human approval needed after {} turns.", n)
                    }
                };
                guardrail_triggered = true;
                break;
            }
        }
    }

    // Use guardrail message as response if triggered, otherwise use LLM response
    let response_text = if guardrail_triggered {
        guardrail_msg
    } else {
        llm_response.content.clone().unwrap_or_default()
    };

    info!(
        agent_id = %agent_id,
        model = %llm_response.model_used,
        tokens = llm_response.tokens.total_tokens,
        guardrail_triggered = guardrail_triggered,
        "LLM response received"
    );

    // 8. Append user message + assistant response to conversation
    append_to_conversation(nats, &conversation_id, "user", &user_text).await;
    append_to_conversation(nats, &conversation_id, "assistant", &response_text).await;

    // 9. If origin exists, publish to outbound channel (skip if guardrail triggered)
    if !guardrail_triggered {
        if let Some(ref origin) = event.origin {
            if !response_text.is_empty() {
                publish_outbound(nats, origin, &response_text).await;
            }
        }
    }

    Ok(())
}

/// Extract human-readable text from a consciousness event's payload.
fn extract_user_text(event: &ConsciousnessEvent) -> String {
    // Try common payload fields in priority order
    if let Some(text) = event.payload.get("text").and_then(|v| v.as_str()) {
        return text.to_string();
    }
    if let Some(msg) = event.payload.get("message").and_then(|v| v.as_str()) {
        return msg.to_string();
    }
    if let Some(reason) = event.payload.get("reason").and_then(|v| v.as_str()) {
        return format!("[{}] {}", event.event_type, reason);
    }

    // Fallback: serialize the event type + payload summary
    format!(
        "[{}] {}",
        event.event_type,
        serde_json::to_string(&event.payload).unwrap_or_else(|_| "{}".to_string())
    )
}

/// Find an existing conversation or create a new one.
async fn find_or_create_conversation(
    nats: &async_nats::Client,
    agent_id: &str,
    agent_def: &AgentDefinition,
    event: &ConsciousnessEvent,
) -> Result<String> {
    // If event already has a conversation_id, use it
    if let Some(ref conv_id) = event.conversation_id {
        if !conv_id.is_empty() {
            debug!(agent_id = %agent_id, conversation_id = %conv_id, "Using existing conversation ID");
            return Ok(conv_id.clone());
        }
    }

    // Determine platform and metadata key for conversation lookup
    let (platform, meta_key, meta_value) = if let Some(ref origin) = event.origin {
        (
            origin.platform.clone(),
            "chat_id".to_string(),
            origin.chat_id.clone(),
        )
    } else if let Some(source) = &event.source_subject {
        // For subscribed events, use the source subject as context
        (
            "internal".to_string(),
            "source_subject".to_string(),
            source.clone(),
        )
    } else {
        (
            "internal".to_string(),
            "event_type".to_string(),
            event.event_type.clone(),
        )
    };

    // Try to find existing conversation
    let find_req = ConversationFindRequest {
        agent_id: agent_id.to_string(),
        platform: platform.clone(),
        metadata_key: meta_key.clone(),
        metadata_value: meta_value.clone(),
        user_id: None,
    };

    let found: serde_json::Value = match tokio::time::timeout(
        Duration::from_secs(5),
        nats_request::<_, serde_json::Value>(nats, "brain.conversation.find", &find_req),
    )
    .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            warn!(error = %e, "conversation.find failed, creating new");
            serde_json::json!(null)
        }
        Err(_) => {
            warn!("conversation.find timed out, creating new");
            serde_json::json!(null)
        }
    };

    // If found, extract ID
    if !found.is_null() {
        if let Some(id) = found.get("id").and_then(|v| v.as_str()) {
            debug!(agent_id = %agent_id, conversation_id = %id, "Found existing conversation");
            return Ok(id.to_string());
        }
    }

    // Create new conversation
    let mut metadata = HashMap::new();
    metadata.insert(meta_key, meta_value);

    let participants = if let Some(ref origin) = event.origin {
        vec![
            format!("user:{}", origin.chat_id),
            format!("agent:{}", agent_id),
        ]
    } else {
        vec![format!("agent:{}", agent_id)]
    };

    let create_req = ConversationCreateRequest {
        platform,
        participants,
        agent_id: agent_id.to_string(),
        scope: agent_def.scope.clone(),
        metadata,
        user_id: None,
    };

    let create_resp: ConversationCreateResponse = tokio::time::timeout(
        Duration::from_secs(5),
        nats_request::<_, ConversationCreateResponse>(
            nats,
            "brain.conversation.create",
            &create_req,
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("conversation.create timed out"))?
    .context("conversation.create failed")?;

    info!(
        agent_id = %agent_id,
        conversation_id = %create_resp.conversation_id,
        "Created new conversation"
    );

    Ok(create_resp.conversation_id)
}

/// Load recent conversation history (last 20 messages).
async fn load_conversation_history(
    nats: &async_nats::Client,
    conversation_id: &str,
) -> Vec<ConversationMessage> {
    let get_req = ConversationGetRequest {
        conversation_id: conversation_id.to_string(),
        max_messages: 20,
        user_id: None,
    };

    let conv: Option<Conversation> = match tokio::time::timeout(
        Duration::from_secs(5),
        nats_request::<_, serde_json::Value>(nats, "brain.conversation.get", &get_req),
    )
    .await
    {
        Ok(Ok(v)) if !v.is_null() => serde_json::from_value(v).ok(),
        _ => None,
    };

    conv.map(|c| c.messages).unwrap_or_default()
}

/// Search for relevant skills based on the event text.
async fn search_relevant_skills(
    nats: &async_nats::Client,
    query: &str,
    scope: &str,
) -> Vec<String> {
    if query.is_empty() {
        return vec![];
    }

    // D1: Pass the agent's scope so skill search respects scope boundaries
    let search_req = SkillSearchRequest {
        query: query.to_string(),
        limit: Some(3),
        scope: Some(scope.to_string()),
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        nats_request::<_, SkillSearchResponse>(nats, "brain.skill.search", &search_req),
    )
    .await
    {
        Ok(Ok(resp)) => resp.skills.into_iter().map(|s| s.snippet).collect(),
        Ok(Err(e)) => {
            debug!(error = %e, "Skill search failed (non-fatal)");
            vec![]
        }
        Err(_) => {
            debug!("Skill search timed out (non-fatal)");
            vec![]
        }
    }
}

/// Build the full system prompt from base prompt + cached agent identity + skill snippets.
#[allow(dead_code)]
fn build_full_system_prompt(
    base_system_prompt: &str,
    agent_def: &AgentDefinition,
    cached_identity: &str,
    skill_snippets: &[String],
) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Base system prompt
    if !base_system_prompt.is_empty() {
        parts.push(base_system_prompt.to_string());
    }

    // S1+S2: Use cached identity prompt instead of reading from disk each time
    if !cached_identity.is_empty() {
        parts.push(format!("## Agent Identity\n\n{}", cached_identity));
    }

    // Agent description
    if let Some(ref desc) = agent_def.description {
        parts.push(format!("## Agent Description\n\n{}", desc));
    }

    // Skill snippets
    if !skill_snippets.is_empty() {
        let skills_section = skill_snippets
            .iter()
            .enumerate()
            .map(|(i, s)| format!("### Skill {}\n{}", i + 1, s))
            .collect::<Vec<_>>()
            .join("\n\n");
        parts.push(format!("## Active Skills\n\n{}", skills_section));
    }

    parts.join("\n\n---\n\n")
}

/// Build the full system prompt including user preferences (Phase 2.3).
fn build_full_system_prompt_with_preferences(
    base_system_prompt: &str,
    agent_def: &AgentDefinition,
    cached_identity: &str,
    skill_snippets: &[String],
    preferences: &[UserPreference],
) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Base system prompt
    if !base_system_prompt.is_empty() {
        parts.push(base_system_prompt.to_string());
    }

    // S1+S2: Use cached identity prompt instead of reading from disk each time
    if !cached_identity.is_empty() {
        parts.push(format!("## Agent Identity\n\n{}", cached_identity));
    }

    // Agent description
    if let Some(ref desc) = agent_def.description {
        parts.push(format!("## Agent Description\n\n{}", desc));
    }

    // Skill snippets
    if !skill_snippets.is_empty() {
        let skills_section = skill_snippets
            .iter()
            .enumerate()
            .map(|(i, s)| format!("### Skill {}\n{}", i + 1, s))
            .collect::<Vec<_>>()
            .join("\n\n");
        parts.push(format!("## Active Skills\n\n{}", skills_section));
    }

    // User preferences (Phase 2.3)
    if !preferences.is_empty() {
        let prefs_section = preferences
            .iter()
            .map(|p| {
                format!(
                    "- {}: {} (confidence: {:.0}%)",
                    p.key,
                    p.value,
                    p.confidence * 100.0
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        parts.push(format!(
            "## User Preferences\nThe following preferences have been learned from past interactions. Apply them:\n{}",
            prefs_section
        ));
    }

    parts.join("\n\n---\n\n")
}

/// Query user preferences for an agent via NATS request/reply (Phase 2.3).
async fn query_user_preferences(
    nats: &async_nats::Client,
    agent_id: &str,
    _conversation_id: &str,
) -> Vec<UserPreference> {
    let prefs_request = PreferencesQueryRequest {
        agent_id: agent_id.to_string(),
        scope: None,
        category: None,
    };

    let prefs_envelope = match EventEnvelope::new(
        "agent.preferences.query",
        "consciousness",
        None,
        None,
        &prefs_request,
    ) {
        Ok(e) => e,
        Err(e) => {
            warn!(agent_id = %agent_id, error = %e, "Failed to create preferences query envelope");
            return Vec::new();
        }
    };

    let prefs_bytes = match serde_json::to_vec(&prefs_envelope) {
        Ok(b) => b,
        Err(e) => {
            warn!(agent_id = %agent_id, error = %e, "Failed to serialize preferences query");
            return Vec::new();
        }
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        nats.request("agent.preferences.query", prefs_bytes.into()),
    )
    .await
    {
        Ok(Ok(resp)) => match serde_json::from_slice::<PreferencesQueryResponse>(&resp.payload) {
            Ok(r) => {
                debug!(
                    agent_id = %agent_id,
                    count = r.preferences.len(),
                    "Loaded user preferences"
                );
                r.preferences
            }
            Err(e) => {
                warn!(agent_id = %agent_id, error = %e, "Failed to deserialize preferences response");
                Vec::new()
            }
        },
        Ok(Err(e)) => {
            debug!(agent_id = %agent_id, error = %e, "Preferences query request failed");
            Vec::new()
        }
        Err(_) => {
            debug!(agent_id = %agent_id, "Preferences query timed out (5s)");
            Vec::new()
        }
    }
}

/// Store a user preference as a fact in the knowledge graph (Phase 2.3).
/// Retries up to 2 times on transient failures.
async fn store_preference_fact(
    nats: &async_nats::Client,
    agent_id: &str,
    preference_key: &str,
    preference_value: &str,
    confidence: f64,
) -> Result<()> {
    let predicate = format!("user_prefers_{}", preference_key);

    let fact_request = FactUpsertRequest {
        subject: agent_id.to_string(),
        predicate,
        object: None,
        value: Some(preference_value.to_string()),
        confidence,
        source_content: format!("User preference: {} = {}", preference_key, preference_value),
        valid_from: Some(Utc::now()),
    };

    let fact_envelope = EventEnvelope::new(
        "brain.fact.upsert",
        "consciousness",
        None,
        None,
        &fact_request,
    )?;

    let fact_bytes = serde_json::to_vec(&fact_envelope)?;

    // Retry up to 2 times on failure
    let max_retries = 2u32;
    let mut last_error = String::new();

    for attempt in 0..=max_retries {
        if attempt > 0 {
            // Exponential backoff: 500ms, 1000ms
            tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
            debug!(
                agent_id = %agent_id,
                key = %preference_key,
                attempt,
                "Retrying preference fact upsert"
            );
        }

        match tokio::time::timeout(
            Duration::from_secs(5),
            nats.request("brain.fact.upsert", fact_bytes.clone().into()),
        )
        .await
        {
            Ok(Ok(_resp)) => {
                debug!(
                    agent_id = %agent_id,
                    key = %preference_key,
                    value = %preference_value,
                    attempt,
                    "Stored preference fact"
                );
                return Ok(());
            }
            Ok(Err(e)) => {
                last_error = format!("NATS request failed: {}", e);
                warn!(
                    agent_id = %agent_id,
                    key = %preference_key,
                    attempt,
                    error = %e,
                    "Failed to store preference fact"
                );
            }
            Err(_) => {
                last_error = "Timed out (5s)".to_string();
                warn!(
                    agent_id = %agent_id,
                    key = %preference_key,
                    attempt,
                    "Preference fact upsert timed out (5s)"
                );
            }
        }
    }

    error!(
        agent_id = %agent_id,
        key = %preference_key,
        retries = max_retries,
        "All preference fact upsert attempts failed"
    );
    Err(anyhow::anyhow!(
        "Preference fact upsert failed after {} retries: {}",
        max_retries,
        last_error
    ))
}

/// Load an agent's identity prompt from its prompt file path.
fn load_agent_identity_prompt(prompt_path: &str) -> String {
    let path = Path::new(prompt_path);
    match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(_) => {
            debug!(path = %prompt_path, "Agent prompt file not found, using empty identity");
            String::new()
        }
    }
}

/// Build the messages array from conversation history + current event.
fn build_messages(
    history: &[ConversationMessage],
    user_text: &str,
    _event: &ConsciousnessEvent,
) -> Vec<UnifiedMessage> {
    let mut messages: Vec<UnifiedMessage> = Vec::new();

    // Add conversation history
    for msg in history {
        messages.push(UnifiedMessage {
            role: msg.role.clone(),
            content: msg.content.clone(),
            tool_calls: if msg.tool_calls.is_empty() {
                None
            } else {
                Some(msg.tool_calls.clone())
            },
            tool_results: if msg.tool_results.is_empty() {
                None
            } else {
                Some(msg.tool_results.clone())
            },
        });
    }

    // Add current user message
    messages.push(UnifiedMessage {
        role: "user".to_string(),
        content: Some(user_text.to_string()),
        tool_calls: None,
        tool_results: None,
    });

    messages
}

/// Build guardrail config, applying per-agent overrides.
fn build_guardrail_config(agent_def: &AgentDefinition) -> seidrum_common::events::GuardrailConfig {
    let mut config = guardrails::default_consciousness();

    if let Some(ref overrides) = agent_def.guardrails {
        if let Some(turn_limit) = overrides.turn_limit {
            config.turn_limit = turn_limit;
        }
        if let Some(time_limit) = overrides.time_limit_seconds {
            config.time_limit_seconds = time_limit;
        }
        if let Some(hitl) = overrides.hitl_after_turns {
            config.hitl_after_turns = Some(hitl);
        }
    }

    config
}

/// Append a message to a conversation.
async fn append_to_conversation(
    nats: &async_nats::Client,
    conversation_id: &str,
    role: &str,
    content: &str,
) {
    if content.is_empty() {
        return;
    }

    let append_req = ConversationAppendRequest {
        conversation_id: conversation_id.to_string(),
        message: ConversationMessage {
            role: role.to_string(),
            content: Some(content.to_string()),
            tool_calls: vec![],
            tool_results: vec![],
            media: vec![],
            timestamp: Utc::now(),
            active_skills: vec![],
        },
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        nats_request::<_, ConversationAppendResponse>(
            nats,
            "brain.conversation.append",
            &append_req,
        ),
    )
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            warn!(
                conversation_id = %conversation_id,
                role = %role,
                error = %e,
                "Failed to append message to conversation"
            );
        }
        Err(_) => {
            warn!(
                conversation_id = %conversation_id,
                role = %role,
                "Timeout appending message to conversation"
            );
        }
    }
}

/// Publish a response to the originating channel.
async fn publish_outbound(nats: &async_nats::Client, origin: &EventOrigin, text: &str) {
    let outbound = ChannelOutbound {
        platform: origin.platform.clone(),
        chat_id: origin.chat_id.clone(),
        text: text.to_string(),
        format: "markdown".to_string(),
        reply_to: origin.message_id.clone(),
        actions: vec![],
    };

    let subject = format!("channel.{}.outbound", origin.platform);
    let envelope = match EventEnvelope::new(&subject, "consciousness", None, None, &outbound) {
        Ok(e) => e,
        Err(e) => {
            error!(error = %e, "Failed to create outbound envelope");
            return;
        }
    };

    match serde_json::to_vec(&envelope) {
        Ok(bytes) => {
            if let Err(e) = nats.publish(subject.clone(), bytes.into()).await {
                error!(subject = %subject, error = %e, "Failed to publish outbound");
            } else {
                debug!(
                    platform = %origin.platform,
                    chat_id = %origin.chat_id,
                    "Published outbound response"
                );
            }
        }
        Err(e) => {
            error!(error = %e, "Failed to serialize outbound envelope");
        }
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

/// Reload agent definitions from disk into Arc<RwLock<HashMap>>.
/// Called when `kernel.agents.reload` is received. Updates the shared agents map
/// and re-caches identity prompts. This allows runtime enable/disable without restart.
async fn reload_agents_internal(
    agents: &Arc<RwLock<HashMap<String, AgentDefinition>>>,
    identity_prompts: &Arc<RwLock<HashMap<String, String>>>,
    agents_dir: &str,
) -> Result<()> {
    // TODO: Full reload should reconcile NATS subscriptions and per-agent tasks.
    // Currently only the in-memory agent definitions are updated. Agents that are
    // newly enabled won't receive events, and agents that are disabled will continue
    // processing until restart. A full fix requires tracking per-agent subscriptions
    // and spawning/stopping forwarder tasks on reload.
    tracing::warn!(
        "Agent reload updated definitions only. New/removed agents require restart to take full effect."
    );

    // Use the agents directory path passed at initialization

    let fresh_agents = load_agents(agents_dir)?;

    // Update the shared agents map
    {
        let mut agents_write = agents.write().await;
        *agents_write = fresh_agents.clone();
    }

    // Rebuild identity prompts cache
    {
        let mut prompts_write = identity_prompts.write().await;
        prompts_write.clear();

        for (agent_id, agent_def) in &fresh_agents {
            let prompt = load_agent_identity_prompt(&agent_def.prompt);
            prompts_write.insert(agent_id.clone(), prompt);
        }
    }

    Ok(())
}

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
                if !def.agent.enabled {
                    info!(
                        agent_id = %def.agent.id,
                        "Consciousness: agent disabled, skipping"
                    );
                    continue;
                }
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
