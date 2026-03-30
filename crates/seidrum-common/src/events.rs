// Seidrum event type definitions.
// All event structs derive Serialize, Deserialize, Debug, Clone.
// Field names match EVENT_CATALOG.md exactly.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Event Origin (for response routing)
// ---------------------------------------------------------------------------

/// Origin channel info for response routing.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EventOrigin {
    pub platform: String,
    pub chat_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Event Envelope
// ---------------------------------------------------------------------------

/// Common wrapper for all NATS events.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EventEnvelope {
    /// ULID
    pub id: String,
    /// Matches NATS subject
    pub event_type: String,
    pub timestamp: DateTime<Utc>,
    /// Plugin that emitted this
    pub source: String,
    /// Links related events
    pub correlation_id: Option<String>,
    /// Scope context
    pub scope: Option<String>,
    /// Event-specific data
    pub payload: serde_json::Value,
    /// Origin channel info for response routing (set by orchestrator).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<EventOrigin>,
}

impl EventEnvelope {
    /// Wrap an arbitrary serializable payload in an envelope.
    pub fn new<T: Serialize>(
        event_type: &str,
        source: &str,
        correlation_id: Option<String>,
        scope: Option<String>,
        payload: &T,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            id: ulid::Ulid::new().to_string(),
            event_type: event_type.to_string(),
            timestamp: Utc::now(),
            source: source.to_string(),
            correlation_id,
            scope,
            payload: serde_json::to_value(payload)?,
            origin: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Channel Events
// ---------------------------------------------------------------------------

/// Incoming user message from any channel plugin.
/// Subject: `channel.{platform}.inbound`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChannelInbound {
    pub platform: String,
    pub user_id: String,
    pub chat_id: String,
    pub text: String,
    pub reply_to: Option<String>,
    pub attachments: Vec<Attachment>,
    pub metadata: HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Attachment {
    pub file_type: String,
    pub url: Option<String>,
    pub file_id: Option<String>,
    pub mime_type: String,
    pub size_bytes: u64,
    /// Base64-encoded content for inline transfer (e.g., images).
    /// Optional to avoid bloating events that only reference files by ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

/// Response from agent back to a channel plugin.
/// Subject: `channel.{platform}.outbound`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChannelOutbound {
    pub platform: String,
    pub chat_id: String,
    pub text: String,
    pub format: String,
    pub reply_to: Option<String>,
    pub actions: Vec<ChannelAction>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChannelAction {
    pub label: String,
    pub action_type: String,
    pub value: String,
}

// ---------------------------------------------------------------------------
// Brain Events
// ---------------------------------------------------------------------------

/// Request to store raw content.
/// Subject: `brain.content.store`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ContentStoreRequest {
    pub content_type: String,
    pub channel: String,
    pub channel_id: String,
    pub raw_text: String,
    pub timestamp: DateTime<Utc>,
    pub metadata: HashMap<String, String>,
    pub generate_embedding: bool,
}

/// Content successfully stored.
/// Subject: `brain.content.stored`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ContentStored {
    pub content_key: String,
    pub content_type: String,
    pub channel: String,
    pub embedding_generated: bool,
    pub timestamp: DateTime<Utc>,
}

/// Create or update an entity.
/// Subject: `brain.entity.upsert`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EntityUpsertRequest {
    /// None = create new, Some = update
    pub entity_key: Option<String>,
    pub entity_type: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub properties: HashMap<String, String>,
    pub source_content: Option<String>,
    /// Content key to create mentions edge
    pub mentions_content: Option<String>,
    pub mention_type: Option<String>,
}

/// Entity created or updated.
/// Subject: `brain.entity.upserted`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EntityUpserted {
    pub entity_key: String,
    pub entity_type: String,
    pub name: String,
    pub is_new: bool,
    pub source_content: Option<String>,
}

/// Create or update a fact.
/// Subject: `brain.fact.upsert`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FactUpsertRequest {
    /// Entity _id
    pub subject: String,
    pub predicate: String,
    /// Entity _id
    pub object: Option<String>,
    /// Literal value
    pub value: Option<String>,
    pub confidence: f64,
    pub source_content: String,
    pub valid_from: Option<DateTime<Utc>>,
}

/// Fact stored.
/// Subject: `brain.fact.upserted`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FactUpserted {
    pub fact_key: String,
    pub subject: String,
    pub predicate: String,
    pub is_new: bool,
    /// Key of fact that was superseded
    pub superseded_fact: Option<String>,
}

/// Assign content/entity to a scope.
/// Subject: `brain.scope.assign`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ScopeAssignRequest {
    /// Entity or content _id
    pub target_key: String,
    pub scope_key: String,
    pub relevance: f64,
}

/// Subject: `brain.scope.assigned`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ScopeAssigned {
    pub target_key: String,
    pub scope_key: String,
}

/// Read query against the brain (request/reply).
/// Subject: `brain.query.request`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BrainQueryRequest {
    pub query_type: String,
    // For AQL:
    pub aql: Option<String>,
    pub bind_vars: Option<HashMap<String, serde_json::Value>>,
    // For vector search:
    pub embedding: Option<Vec<f64>>,
    pub collection: Option<String>,
    pub limit: Option<u32>,
    // For graph traversal:
    pub start_vertex: Option<String>,
    pub direction: Option<String>,
    pub depth: Option<u32>,
    // For get_context (high-level convenience):
    pub query_text: Option<String>,
    pub max_facts: Option<u32>,
    pub graph_depth: Option<u32>,
    pub min_confidence: Option<f64>,
}

/// Subject: `brain.query.response`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BrainQueryResponse {
    pub results: serde_json::Value,
    pub count: u32,
    pub scopes_applied: Vec<String>,
    pub duration_ms: u64,
}

// ---------------------------------------------------------------------------
// Agent Events
// ---------------------------------------------------------------------------

/// Graph context loader has assembled context for a message.
/// Subject: `agent.context.loaded`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentContextLoaded {
    pub original_event: EventEnvelope,
    #[serde(default)]
    pub entities: Vec<serde_json::Value>,
    #[serde(default)]
    pub facts: Vec<serde_json::Value>,
    #[serde(default)]
    pub similar_content: Vec<serde_json::Value>,
    #[serde(default)]
    pub active_tasks: Vec<serde_json::Value>,
    #[serde(default)]
    pub conversation_history: Vec<serde_json::Value>,
    #[serde(default)]
    pub skill_snippets: Vec<serde_json::Value>,
}

/// Explicitly wake an agent.
/// Subject: `agent.{agent_id}.wake`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentWake {
    pub agent_id: String,
    pub reason: String,
    pub context: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// LLM Events
// ---------------------------------------------------------------------------

/// Request for LLM completion.
/// Subject: `llm.request.{provider}` or `llm.request.auto`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LlmRequest {
    pub agent_id: String,
    pub messages: Vec<LlmMessage>,
    pub model: Option<String>,
    pub temperature: f64,
    pub max_tokens: u32,
    pub tools: Option<Vec<ToolSchema>>,
    pub tool_choice: Option<String>,
    pub routing_strategy: String,
    pub model_preferences: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LlmMessage {
    pub role: String,
    pub content: String,
    pub name: Option<String>,
    pub tool_call_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// LLM completion result.
/// Subject: `llm.response`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LlmResponse {
    pub agent_id: String,
    pub content: Option<String>,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub model_used: String,
    pub provider: String,
    pub tokens: TokenUsage,
    pub duration_ms: u64,
    pub finish_reason: String,
    /// Number of tool call rounds executed by the provider.
    #[serde(default)]
    pub tool_rounds: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub function_name: String,
    /// JSON string
    pub arguments: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub estimated_cost_usd: f64,
}

// ---------------------------------------------------------------------------
// Task Events
// ---------------------------------------------------------------------------

/// Subject: `task.created`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TaskCreated {
    pub task_key: String,
    pub title: String,
    pub description: Option<String>,
    pub priority: String,
    pub assigned_agent: Option<String>,
    pub due_date: Option<DateTime<Utc>>,
    pub callback_channel: Option<String>,
    pub scope: String,
}

/// Subject: `task.updated`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TaskUpdated {
    pub task_key: String,
    pub old_status: String,
    pub new_status: String,
    pub update_reason: Option<String>,
}

/// Subject: `task.completed.{task_id}`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TaskCompleted {
    pub task_key: String,
    pub result: Option<String>,
    pub duration_ms: u64,
    pub callback_channel: Option<String>,
}

// ---------------------------------------------------------------------------
// Feedback Events
// ---------------------------------------------------------------------------

/// Type of feedback the user is providing.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackType {
    /// User explicitly corrects the agent ("no, I meant...")
    Correction,
    /// User confirms something non-obvious ("yes, exactly")
    Confirmation,
    /// User expresses a preference ("I prefer shorter responses")
    Preference,
    /// User provides a rating or evaluation
    Rating,
}

/// Feedback extracted from user interaction.
/// Subject: `agent.feedback`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentFeedback {
    pub agent_id: String,
    pub conversation_id: Option<String>,
    pub message_id: Option<String>,
    pub feedback_type: FeedbackType,
    /// What the user said that constitutes feedback
    pub content: String,
    /// The agent's prior response that prompted this feedback
    pub prior_context: Option<String>,
    /// Structured preference if extractable (e.g., "response_length": "short")
    pub preference_key: Option<String>,
    pub preference_value: Option<String>,
    /// Confidence that this is actually feedback (0.0-1.0)
    pub confidence: f64,
    pub timestamp: DateTime<Utc>,
}

/// User preferences aggregated from feedback.
/// Subject: `agent.preferences.query` (request/reply)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PreferencesQueryRequest {
    pub agent_id: String,
    pub scope: Option<String>,
    /// Optional filter by preference category
    pub category: Option<String>,
}

/// Response with active preferences.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PreferencesQueryResponse {
    pub preferences: Vec<UserPreference>,
}

/// A single user preference derived from feedback.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UserPreference {
    pub key: String,
    pub value: String,
    pub confidence: f64,
    pub source_feedback_count: u32,
    pub last_updated: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Plugin Events
// ---------------------------------------------------------------------------

/// Plugin announces itself to the kernel.
/// Subject: `plugin.register`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PluginRegister {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    /// NATS subjects this plugin consumes
    pub consumes: Vec<String>,
    /// NATS subjects this plugin produces
    pub produces: Vec<String>,
    pub health_subject: String,
    /// Event types this plugin consumes (e.g., "ChannelInbound").
    #[serde(default)]
    pub consumed_event_types: Vec<String>,
    /// Event types this plugin produces (e.g., "LlmResponse").
    #[serde(default)]
    pub produced_event_types: Vec<String>,
    /// JSON Schema describing this plugin's configurable parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
}

/// Notification that a plugin's configuration was updated.
/// Subject: `plugin.{id}.config.update` (publish)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConfigUpdated {
    pub plugin_id: String,
    pub config: serde_json::Value,
    pub updated_by: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Plugin announces it is shutting down.
/// Subject: `plugin.deregister`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PluginDeregister {
    pub id: String,
}

/// Subject: `plugin.{id}.health` (request)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PluginHealthRequest {}

/// Subject: `plugin.{id}.health` (reply)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PluginHealthResponse {
    pub plugin_id: String,
    pub status: String,
    pub uptime_seconds: u64,
    pub events_processed: u64,
    pub last_error: Option<String>,
}

/// Subject: `plugin.{id}.error`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PluginError {
    pub plugin_id: String,
    pub error_type: String,
    pub message: String,
    pub event_id: String,
    pub recoverable: bool,
}

// ---------------------------------------------------------------------------
// System Events
// ---------------------------------------------------------------------------

/// Subject: `system.health`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SystemHealth {
    pub nats_connected: bool,
    pub arangodb_connected: bool,
    pub active_plugins: Vec<String>,
    pub active_agents: u32,
    pub uptime_seconds: u64,
}

/// Subject: `system.maintenance.decay`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DecayCompleted {
    pub facts_decayed: u32,
    pub facts_archived: u32,
    pub duration_ms: u64,
}

// ---------------------------------------------------------------------------
// Tool Registry Events
// ---------------------------------------------------------------------------

/// Plugin registers a tool/capability with the kernel.
/// Subject: `capability.register`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolRegister {
    pub tool_id: String,
    pub plugin_id: String,
    pub name: String,
    pub summary_md: String,
    pub manual_md: String,
    pub parameters: serde_json::Value,
    pub call_subject: String,
    /// Capability kind: "tool", "command", "both", or future types.
    /// Defaults to "tool" for backward compatibility.
    #[serde(default = "default_kind")]
    pub kind: String,
}

fn default_kind() -> String {
    "tool".to_string()
}

/// Kernel confirms tool registration.
/// Subject: `tool.registered`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolRegistered {
    pub tool_id: String,
    pub plugin_id: String,
    pub name: String,
}

/// Search for tools/capabilities by query text.
/// Subject: `capability.search`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolSearchRequest {
    pub query_text: String,
    pub limit: Option<u32>,
    /// Optional filter by capability kind (e.g., "tool", "command", "both").
    #[serde(default)]
    pub kind_filter: Option<String>,
}

/// Summary of a tool/capability returned in search results.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolSummary {
    pub tool_id: String,
    pub name: String,
    pub summary_md: String,
    pub parameters: serde_json::Value,
    /// Capability kind: "tool", "command", "both".
    pub kind: String,
}

/// Subject: `tool.search.response`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolSearchResponse {
    pub tools: Vec<ToolSummary>,
}

/// Request full tool description.
/// Subject: `tool.describe.request`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolDescribeRequest {
    pub tool_id: String,
}

/// Full tool/capability description.
/// Subject: `capability.describe` (response)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolDescribeResponse {
    pub tool_id: String,
    pub name: String,
    pub summary_md: String,
    pub manual_md: String,
    pub parameters: serde_json::Value,
    pub plugin_id: String,
    pub call_subject: String,
    /// Capability kind: "tool", "command", "both".
    pub kind: String,
}

/// Request to invoke a tool.
/// Subject: `tool.call`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolCallRequest {
    pub tool_id: String,
    pub plugin_id: String,
    pub arguments: serde_json::Value,
    pub correlation_id: Option<String>,
}

/// Result of a tool invocation.
/// Subject: `tool.call` (reply)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolCallResponse {
    pub tool_id: String,
    pub result: serde_json::Value,
    pub is_error: bool,
}

// ---------------------------------------------------------------------------
// Unified LLM Events (provider-agnostic)
// ---------------------------------------------------------------------------

/// Unified message format, not tied to any provider.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UnifiedMessage {
    pub role: String,
    pub content: Option<String>,
    pub tool_calls: Option<Vec<UnifiedToolCall>>,
    pub tool_results: Option<Vec<UnifiedToolResult>>,
}

/// A tool call within a unified message.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UnifiedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// A tool result within a unified message.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UnifiedToolResult {
    pub tool_call_id: String,
    pub content: String,
    pub is_error: bool,
}

/// Provider-agnostic LLM request.
/// Subject: `llm.request`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UnifiedLlmRequest {
    pub agent_id: String,
    pub messages: Vec<UnifiedMessage>,
    pub system_prompt: Option<String>,
    pub tools: Vec<ToolSchema>,
    pub config: LlmCallConfig,
    pub routing_strategy: String,
    pub model_preferences: Vec<String>,
    pub correlation_id: Option<String>,
    pub scope: Option<String>,
}

/// Configuration for an LLM call.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LlmCallConfig {
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub top_p: Option<f64>,
}

// ---------------------------------------------------------------------------
// Plugin Storage Events
// ---------------------------------------------------------------------------

/// Request to get a value from plugin storage.
/// Subject: `storage.get` (request/reply)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StorageGetRequest {
    pub plugin_id: String,
    pub namespace: String,
    pub key: String,
}

/// Response to a storage get request.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StorageGetResponse {
    pub found: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
}

/// Request to set a value in plugin storage.
/// Subject: `storage.set` (request/reply)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StorageSetRequest {
    pub plugin_id: String,
    pub namespace: String,
    pub key: String,
    pub value: serde_json::Value,
}

/// Response to a storage set request.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StorageSetResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Request to delete a value from plugin storage.
/// Subject: `storage.delete` (request/reply)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StorageDeleteRequest {
    pub plugin_id: String,
    pub namespace: String,
    pub key: String,
}

/// Response to a storage delete request.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StorageDeleteResponse {
    pub success: bool,
    pub existed: bool,
}

/// Request to list keys in a plugin storage namespace.
/// Subject: `storage.list` (request/reply)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StorageListRequest {
    pub plugin_id: String,
    pub namespace: String,
}

/// Response to a storage list request.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StorageListResponse {
    pub keys: Vec<String>,
}

// ---------------------------------------------------------------------------
// Conversation Events
// ---------------------------------------------------------------------------

/// A media attachment within a conversation message.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MediaAttachment {
    /// "image" | "voice" | "file" | "video"
    pub media_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub mime_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
}

/// A single message within a conversation.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConversationMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<UnifiedToolCall>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_results: Vec<UnifiedToolResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<MediaAttachment>,
    pub timestamp: DateTime<Utc>,
    /// Skills that were active during this message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_skills: Vec<String>,
}

/// Request to create a new conversation.
/// Subject: `brain.conversation.create` (request/reply)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConversationCreateRequest {
    pub platform: String,
    pub participants: Vec<String>,
    pub agent_id: String,
    pub scope: String,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

/// Response to conversation create.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConversationCreateResponse {
    pub conversation_id: String,
}

/// Request to append a message to a conversation.
/// Subject: `brain.conversation.append` (request/reply)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConversationAppendRequest {
    pub conversation_id: String,
    pub message: ConversationMessage,
}

/// Response to conversation append.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConversationAppendResponse {
    pub success: bool,
    pub message_count: u32,
}

/// Request to get a conversation by ID.
/// Subject: `brain.conversation.get` (request/reply)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConversationGetRequest {
    pub conversation_id: String,
    /// Maximum number of recent messages to return (0 = all).
    #[serde(default)]
    pub max_messages: u32,
}

/// Full conversation document.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Conversation {
    pub id: String,
    pub platform: String,
    pub participants: Vec<String>,
    pub agent_id: String,
    pub scope: String,
    pub messages: Vec<ConversationMessage>,
    pub metadata: HashMap<String, String>,
    pub state: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Request to find a conversation by platform metadata.
/// Subject: `brain.conversation.find` (request/reply)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConversationFindRequest {
    pub agent_id: String,
    pub platform: String,
    pub metadata_key: String,
    pub metadata_value: String,
}

/// Request to list conversations.
/// Subject: `brain.conversation.list` (request/reply)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConversationListRequest {
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default)]
    pub limit: u32,
}

/// Response to conversation list.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConversationListResponse {
    pub conversations: Vec<ConversationSummary>,
}

/// Summary of a conversation for listing.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConversationSummary {
    pub id: String,
    pub platform: String,
    pub participants: Vec<String>,
    pub message_count: u32,
    pub state: String,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Consciousness Events
// ---------------------------------------------------------------------------

/// An event delivered to an agent's consciousness stream.
/// Subject: `agent.{id}.consciousness`
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConsciousnessEvent {
    pub agent_id: String,
    /// "user_message" | "subscribed_event" | "self_wake" | "agent_message" | "delegation_result"
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    pub payload: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<EventOrigin>,
}

/// Request to subscribe an agent to NATS event patterns at runtime.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SubscribeEventsRequest {
    pub subjects: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<u64>,
}

/// Request to unsubscribe an agent from NATS event patterns.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UnsubscribeEventsRequest {
    pub subjects: Vec<String>,
}

/// Request to delegate a task to another agent.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DelegateTaskRequest {
    pub to_agent_id: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
}

/// Request to schedule a self-wake timer.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ScheduleWakeRequest {
    pub delay_seconds: u64,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
}

/// Request to send a proactive notification to a user channel.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SendNotificationRequest {
    pub channel: String,
    pub chat_id: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
}

/// Guardrail configuration for tool loop limits.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GuardrailConfig {
    pub turn_limit: u32,
    pub time_limit_seconds: u64,
    pub loop_detection_threshold: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hitl_after_turns: Option<u32>,
}

impl Default for GuardrailConfig {
    fn default() -> Self {
        Self {
            turn_limit: 50,
            time_limit_seconds: 600,
            loop_detection_threshold: 3,
            hitl_after_turns: Some(20),
        }
    }
}

// ---------------------------------------------------------------------------
// Skill Events
// ---------------------------------------------------------------------------

/// Request to search skills by semantic similarity.
/// Subject: `brain.skill.search` (request/reply)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SkillSearchRequest {
    pub query: String,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// A single skill search result.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SkillSearchResult {
    pub id: String,
    pub description: String,
    pub snippet: String,
    pub score: f64,
    pub source: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Response to skill search.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SkillSearchResponse {
    pub skills: Vec<SkillSearchResult>,
}

/// Request to save a skill.
/// Subject: `brain.skill.save` (request/reply)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SkillSaveRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub description: String,
    pub snippet: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub learned_from: Option<String>,
    /// Pre-computed embedding vector (if available).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embedding: Vec<f64>,
}

/// Response to skill save.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SkillSaveResponse {
    pub skill_id: String,
    pub is_new: bool,
}

/// Request to get a skill by ID.
/// Subject: `brain.skill.get` (request/reply)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SkillGetRequest {
    pub skill_id: String,
}

/// Request to list skills.
/// Subject: `brain.skill.list` (request/reply)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SkillListRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_filter: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Response to skill list.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SkillListResponse {
    pub skills: Vec<SkillSearchResult>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_event_envelope() {
        let envelope = EventEnvelope {
            id: ulid::Ulid::new().to_string(),
            event_type: "channel.telegram.inbound".to_string(),
            timestamp: Utc::now(),
            source: "plugin-telegram".to_string(),
            correlation_id: Some("corr-123".to_string()),
            scope: Some("personal".to_string()),
            payload: serde_json::json!({"key": "value"}),
            origin: None,
        };

        let json = serde_json::to_string(&envelope).unwrap();
        let deserialized: EventEnvelope = serde_json::from_str(&json).unwrap();

        assert_eq!(envelope.id, deserialized.id);
        assert_eq!(envelope.event_type, deserialized.event_type);
        assert_eq!(envelope.source, deserialized.source);
        assert_eq!(envelope.correlation_id, deserialized.correlation_id);
        assert_eq!(envelope.scope, deserialized.scope);
    }

    #[test]
    fn roundtrip_channel_inbound() {
        let event = ChannelInbound {
            platform: "telegram".to_string(),
            user_id: "user-42".to_string(),
            chat_id: "chat-99".to_string(),
            text: "Hello, agent!".to_string(),
            reply_to: None,
            attachments: vec![Attachment {
                file_type: "image".to_string(),
                url: Some("https://example.com/img.png".to_string()),
                file_id: None,
                mime_type: "image/png".to_string(),
                size_bytes: 1024,
                data: None,
            }],
            metadata: HashMap::from([("lang".to_string(), "en".to_string())]),
        };

        let json = serde_json::to_string(&event).unwrap();
        let deserialized: ChannelInbound = serde_json::from_str(&json).unwrap();

        assert_eq!(event.platform, deserialized.platform);
        assert_eq!(event.user_id, deserialized.user_id);
        assert_eq!(event.text, deserialized.text);
        assert_eq!(event.attachments.len(), deserialized.attachments.len());
        assert_eq!(
            event.attachments[0].size_bytes,
            deserialized.attachments[0].size_bytes
        );
    }

    #[test]
    fn roundtrip_llm_request() {
        let event = LlmRequest {
            agent_id: "agent-main".to_string(),
            messages: vec![LlmMessage {
                role: "user".to_string(),
                content: "What is Seidrum?".to_string(),
                name: None,
                tool_call_id: None,
            }],
            model: None,
            temperature: 0.7,
            max_tokens: 2048,
            tools: Some(vec![ToolSchema {
                name: "brain_query".to_string(),
                description: "Query the knowledge graph".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }]),
            tool_choice: Some("auto".to_string()),
            routing_strategy: "best-first".to_string(),
            model_preferences: vec!["gpt-4".to_string(), "claude-3".to_string()],
        };

        let json = serde_json::to_string(&event).unwrap();
        let deserialized: LlmRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(event.agent_id, deserialized.agent_id);
        assert_eq!(event.messages.len(), deserialized.messages.len());
        assert_eq!(event.temperature, deserialized.temperature);
        assert_eq!(event.routing_strategy, deserialized.routing_strategy);
        assert!(deserialized.tools.is_some());
        assert_eq!(deserialized.tools.unwrap().len(), 1);
    }

    #[test]
    fn roundtrip_brain_query_request() {
        let event = BrainQueryRequest {
            query_type: "get_context".to_string(),
            aql: None,
            bind_vars: None,
            embedding: None,
            collection: None,
            limit: Some(10),
            start_vertex: None,
            direction: None,
            depth: None,
            query_text: Some("Tell me about Rust".to_string()),
            max_facts: Some(20),
            graph_depth: Some(2),
            min_confidence: Some(0.5),
        };

        let json = serde_json::to_string(&event).unwrap();
        let deserialized: BrainQueryRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(event.query_type, deserialized.query_type);
        assert_eq!(event.query_text, deserialized.query_text);
        assert_eq!(event.max_facts, deserialized.max_facts);
        assert_eq!(event.min_confidence, deserialized.min_confidence);
    }

    #[test]
    fn envelope_new_helper() {
        let inbound = ChannelInbound {
            platform: "cli".to_string(),
            user_id: "u1".to_string(),
            chat_id: "c1".to_string(),
            text: "hi".to_string(),
            reply_to: None,
            attachments: vec![],
            metadata: HashMap::new(),
        };

        let envelope =
            EventEnvelope::new("channel.cli.inbound", "cli-plugin", None, None, &inbound).unwrap();

        assert_eq!(envelope.event_type, "channel.cli.inbound");
        assert_eq!(envelope.source, "cli-plugin");
        assert!(!envelope.id.is_empty());

        // Payload should deserialize back to ChannelInbound
        let recovered: ChannelInbound = serde_json::from_value(envelope.payload).unwrap();
        assert_eq!(recovered.platform, "cli");
    }

    #[test]
    fn roundtrip_tool_register() {
        let event = ToolRegister {
            tool_id: "tool-001".to_string(),
            plugin_id: "plugin-code-exec".to_string(),
            name: "run_python".to_string(),
            summary_md: "Execute Python code in a sandbox".to_string(),
            manual_md: "# run_python\nRuns Python 3 code.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "code": {"type": "string"}
                },
                "required": ["code"]
            }),
            call_subject: "plugin.code-exec.tool.run_python".to_string(),
            kind: "tool".to_string(),
        };

        let json = serde_json::to_string(&event).unwrap();
        let deserialized: ToolRegister = serde_json::from_str(&json).unwrap();

        assert_eq!(event.tool_id, deserialized.tool_id);
        assert_eq!(event.plugin_id, deserialized.plugin_id);
        assert_eq!(event.name, deserialized.name);
        assert_eq!(event.call_subject, deserialized.call_subject);
        assert_eq!(event.parameters, deserialized.parameters);
        assert_eq!(event.kind, deserialized.kind);
    }

    #[test]
    fn tool_register_kind_defaults_to_tool() {
        // Simulate an old-style registration without the `kind` field
        let json = r##"{
            "tool_id": "tool-old",
            "plugin_id": "plugin-old",
            "name": "old_tool",
            "summary_md": "An old tool",
            "manual_md": "# old_tool",
            "parameters": {},
            "call_subject": "tool.call.old"
        }"##;
        let deserialized: ToolRegister = serde_json::from_str(json).unwrap();
        assert_eq!(deserialized.kind, "tool");
    }

    #[test]
    fn roundtrip_tool_call_request() {
        let event = ToolCallRequest {
            tool_id: "tool-001".to_string(),
            plugin_id: "plugin-code-exec".to_string(),
            arguments: serde_json::json!({"code": "print('hello')"}),
            correlation_id: Some("corr-456".to_string()),
        };

        let json = serde_json::to_string(&event).unwrap();
        let deserialized: ToolCallRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(event.tool_id, deserialized.tool_id);
        assert_eq!(event.plugin_id, deserialized.plugin_id);
        assert_eq!(event.arguments, deserialized.arguments);
        assert_eq!(event.correlation_id, deserialized.correlation_id);
    }

    #[test]
    fn roundtrip_tool_call_response() {
        let event = ToolCallResponse {
            tool_id: "tool-001".to_string(),
            result: serde_json::json!({"stdout": "hello\n", "exit_code": 0}),
            is_error: false,
        };

        let json = serde_json::to_string(&event).unwrap();
        let deserialized: ToolCallResponse = serde_json::from_str(&json).unwrap();

        assert_eq!(event.tool_id, deserialized.tool_id);
        assert_eq!(event.result, deserialized.result);
        assert_eq!(event.is_error, deserialized.is_error);
    }

    #[test]
    fn roundtrip_unified_message() {
        let msg = UnifiedMessage {
            role: "assistant".to_string(),
            content: Some("Let me check that.".to_string()),
            tool_calls: Some(vec![UnifiedToolCall {
                id: "tc-1".to_string(),
                name: "brain_query".to_string(),
                arguments: serde_json::json!({"query": "Rust projects"}),
            }]),
            tool_results: None,
        };

        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: UnifiedMessage = serde_json::from_str(&json).unwrap();

        assert_eq!(msg.role, deserialized.role);
        assert_eq!(msg.content, deserialized.content);
        assert!(deserialized.tool_calls.is_some());
        let calls = deserialized.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "brain_query");
        assert!(deserialized.tool_results.is_none());
    }

    #[test]
    fn roundtrip_unified_llm_request() {
        let event = UnifiedLlmRequest {
            agent_id: "agent-main".to_string(),
            messages: vec![UnifiedMessage {
                role: "user".to_string(),
                content: Some("What is Seidrum?".to_string()),
                tool_calls: None,
                tool_results: None,
            }],
            system_prompt: Some("You are a helpful assistant.".to_string()),
            tools: vec![ToolSchema {
                name: "brain_query".to_string(),
                description: "Query the knowledge graph".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            config: LlmCallConfig {
                temperature: Some(0.7),
                max_tokens: Some(2048),
                top_p: None,
            },
            routing_strategy: "best-first".to_string(),
            model_preferences: vec!["gemini-2.5-pro".to_string()],
            correlation_id: Some("corr-789".to_string()),
            scope: Some("personal".to_string()),
        };

        let json = serde_json::to_string(&event).unwrap();
        let deserialized: UnifiedLlmRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(event.agent_id, deserialized.agent_id);
        assert_eq!(event.messages.len(), deserialized.messages.len());
        assert_eq!(event.system_prompt, deserialized.system_prompt);
        assert_eq!(event.tools.len(), deserialized.tools.len());
        assert_eq!(event.config.temperature, deserialized.config.temperature);
        assert_eq!(event.config.max_tokens, deserialized.config.max_tokens);
        assert!(deserialized.config.top_p.is_none());
        assert_eq!(event.routing_strategy, deserialized.routing_strategy);
        assert_eq!(event.model_preferences, deserialized.model_preferences);
        assert_eq!(event.correlation_id, deserialized.correlation_id);
        assert_eq!(event.scope, deserialized.scope);
    }

    #[test]
    fn roundtrip_skill_search_request() {
        let req = SkillSearchRequest {
            query: "code review".to_string(),
            limit: Some(5),
            scope: Some("scope_root".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let de: SkillSearchRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req.query, de.query);
        assert_eq!(req.limit, de.limit);
        assert_eq!(req.scope, de.scope);
    }

    #[test]
    fn roundtrip_skill_save_request() {
        let req = SkillSaveRequest {
            id: Some("test-skill".to_string()),
            description: "A test skill".to_string(),
            snippet: "Do things correctly".to_string(),
            source: "learned".to_string(),
            scope: None,
            tags: vec!["test".to_string()],
            learned_from: Some("conv_123".to_string()),
            embedding: vec![0.1, 0.2, 0.3],
        };
        let json = serde_json::to_string(&req).unwrap();
        let de: SkillSaveRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req.description, de.description);
        assert_eq!(req.snippet, de.snippet);
        assert_eq!(req.embedding.len(), de.embedding.len());
    }

    #[test]
    fn roundtrip_consciousness_event() {
        let event = ConsciousnessEvent {
            agent_id: "personal-assistant".to_string(),
            event_type: "user_message".to_string(),
            source_subject: Some("channel.telegram.inbound".to_string()),
            conversation_id: Some("conv_123".to_string()),
            payload: serde_json::json!({"text": "hello"}),
            origin: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let de: ConsciousnessEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.agent_id, de.agent_id);
        assert_eq!(event.event_type, de.event_type);
        assert_eq!(event.conversation_id, de.conversation_id);
    }

    #[test]
    fn conversation_message_with_active_skills() {
        let msg = ConversationMessage {
            role: "assistant".to_string(),
            content: Some("I'll review this code.".to_string()),
            tool_calls: vec![],
            tool_results: vec![],
            media: vec![],
            timestamp: Utc::now(),
            active_skills: vec!["code-review".to_string()],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let de: ConversationMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(de.active_skills, vec!["code-review"]);
    }

    #[test]
    fn conversation_message_without_active_skills() {
        // Old format without active_skills should still deserialize
        let json = r#"{"role":"user","content":"hello","timestamp":"2026-03-24T10:00:00Z"}"#;
        let msg: ConversationMessage = serde_json::from_str(json).unwrap();
        assert!(msg.active_skills.is_empty());
    }

    #[test]
    fn roundtrip_agent_feedback() {
        let feedback = AgentFeedback {
            agent_id: "personal-assistant".to_string(),
            conversation_id: Some("conv_456".to_string()),
            message_id: Some("msg_789".to_string()),
            feedback_type: FeedbackType::Correction,
            content: "no, I meant Python not JavaScript".to_string(),
            prior_context: Some("I suggested using JavaScript for that task".to_string()),
            preference_key: Some("language".to_string()),
            preference_value: Some("python".to_string()),
            confidence: 0.95,
            timestamp: Utc::now(),
        };

        let json = serde_json::to_string(&feedback).unwrap();
        let deserialized: AgentFeedback = serde_json::from_str(&json).unwrap();

        assert_eq!(feedback.agent_id, deserialized.agent_id);
        assert_eq!(feedback.feedback_type, deserialized.feedback_type);
        assert_eq!(feedback.content, deserialized.content);
        assert_eq!(feedback.confidence, deserialized.confidence);
        assert_eq!(feedback.preference_key, deserialized.preference_key);
    }

    #[test]
    fn roundtrip_preferences_query_request() {
        let req = PreferencesQueryRequest {
            agent_id: "personal-assistant".to_string(),
            scope: Some("work".to_string()),
            category: Some("communication_style".to_string()),
        };

        let json = serde_json::to_string(&req).unwrap();
        let deserialized: PreferencesQueryRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(req.agent_id, deserialized.agent_id);
        assert_eq!(req.scope, deserialized.scope);
        assert_eq!(req.category, deserialized.category);
    }

    #[test]
    fn roundtrip_user_preference() {
        let pref = UserPreference {
            key: "response_length".to_string(),
            value: "concise".to_string(),
            confidence: 0.88,
            source_feedback_count: 5,
            last_updated: Utc::now(),
        };

        let json = serde_json::to_string(&pref).unwrap();
        let deserialized: UserPreference = serde_json::from_str(&json).unwrap();

        assert_eq!(pref.key, deserialized.key);
        assert_eq!(pref.value, deserialized.value);
        assert_eq!(pref.confidence, deserialized.confidence);
        assert_eq!(
            pref.source_feedback_count,
            deserialized.source_feedback_count
        );
    }

    #[test]
    fn feedback_type_serialization() {
        let correction = FeedbackType::Correction;
        let json = serde_json::to_string(&correction).unwrap();
        assert_eq!(json, "\"correction\"");

        let confirmation = FeedbackType::Confirmation;
        let json = serde_json::to_string(&confirmation).unwrap();
        assert_eq!(json, "\"confirmation\"");

        let preference = FeedbackType::Preference;
        let json = serde_json::to_string(&preference).unwrap();
        assert_eq!(json, "\"preference\"");

        let rating = FeedbackType::Rating;
        let json = serde_json::to_string(&rating).unwrap();
        assert_eq!(json, "\"rating\"");
    }
}
