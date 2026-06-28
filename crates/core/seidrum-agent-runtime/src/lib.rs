use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use redb::{Database, ReadableTable};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    #[error("provider error: {0}")]
    Provider(String),
    #[error("tool `{tool_name}` failed: {message}")]
    Tool { tool_name: String, message: String },
    #[error("store error: {0}")]
    Store(String),
    #[error("tool `{0}` is not registered")]
    ToolNotRegistered(String),
    #[error("provider did not return a final response within {0} iterations")]
    IterationLimit(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnInput {
    pub session_id: String,
    pub agent_id: String,
    pub user_message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnOutput {
    pub session_id: String,
    pub agent_id: String,
    pub final_text: String,
    pub events: Vec<RuntimeEvent>,
    pub tool_results: Vec<ToolResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeEvent {
    OutboundResponse {
        session_id: String,
        agent_id: String,
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub tool_name: String,
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderRequest {
    pub session_id: String,
    pub agent_id: String,
    pub messages: Vec<ContextMessage>,
    pub tool_results: Vec<ToolResult>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderResponse {
    pub final_text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

impl ProviderResponse {
    pub fn final_text(text: impl Into<String>) -> Self {
        Self {
            final_text: Some(text.into()),
            tool_calls: Vec::new(),
        }
    }

    pub fn tool_calls(calls: impl IntoIterator<Item = ToolCall>) -> Self {
        Self {
            final_text: None,
            tool_calls: calls.into_iter().collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub max_tool_iterations: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_tool_iterations: 4,
        }
    }
}

#[async_trait]
pub trait AgentProvider: Send + Sync + Clone + 'static {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, RuntimeError>;
}

#[async_trait]
pub trait ToolExecutor: Send + Sync + 'static {
    async fn execute(&self, call: ToolCall) -> Result<ToolResult, RuntimeError>;
}

#[async_trait]
pub trait RuntimeStore: Send + Sync + Clone + 'static {
    async fn append(&self, record: StoredTurnRecord) -> Result<(), RuntimeError>;
    async fn records(&self, session_id: &str) -> Result<Vec<StoredTurnRecord>, RuntimeError>;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StoredTurnRecord {
    UserMessage {
        record_id: String,
        sequence: u64,
        session_id: String,
        agent_id: String,
        content: String,
    },
    AssistantToolCall {
        record_id: String,
        sequence: u64,
        session_id: String,
        agent_id: String,
        call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        record_id: String,
        sequence: u64,
        session_id: String,
        agent_id: String,
        call_id: String,
        tool_name: String,
        content: String,
        is_error: bool,
    },
    AssistantMessage {
        record_id: String,
        sequence: u64,
        session_id: String,
        agent_id: String,
        content: String,
    },
}

impl StoredTurnRecord {
    pub fn record_id(&self) -> &str {
        match self {
            Self::UserMessage { record_id, .. }
            | Self::AssistantToolCall { record_id, .. }
            | Self::ToolResult { record_id, .. }
            | Self::AssistantMessage { record_id, .. } => record_id,
        }
    }

    pub fn sequence(&self) -> u64 {
        match self {
            Self::UserMessage { sequence, .. }
            | Self::AssistantToolCall { sequence, .. }
            | Self::ToolResult { sequence, .. }
            | Self::AssistantMessage { sequence, .. } => *sequence,
        }
    }

    pub fn session_id(&self) -> &str {
        match self {
            Self::UserMessage { session_id, .. }
            | Self::AssistantToolCall { session_id, .. }
            | Self::ToolResult { session_id, .. }
            | Self::AssistantMessage { session_id, .. } => session_id,
        }
    }

    pub fn agent_id(&self) -> &str {
        match self {
            Self::UserMessage { agent_id, .. }
            | Self::AssistantToolCall { agent_id, .. }
            | Self::ToolResult { agent_id, .. }
            | Self::AssistantMessage { agent_id, .. } => agent_id,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryTurnStore {
    inner: Arc<Mutex<InMemoryTurnStoreInner>>,
}

#[derive(Debug, Default)]
struct InMemoryTurnStoreInner {
    records_by_session: HashMap<String, Vec<StoredTurnRecord>>,
}

#[async_trait]
impl RuntimeStore for InMemoryTurnStore {
    async fn append(&self, record: StoredTurnRecord) -> Result<(), RuntimeError> {
        let session_id = record.session_id().to_string();
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| RuntimeError::Store("in-memory store lock poisoned".to_string()))?;
        inner
            .records_by_session
            .entry(session_id)
            .or_default()
            .push(record);
        Ok(())
    }

    async fn records(&self, session_id: &str) -> Result<Vec<StoredTurnRecord>, RuntimeError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| RuntimeError::Store("in-memory store lock poisoned".to_string()))?;
        Ok(inner
            .records_by_session
            .get(session_id)
            .cloned()
            .unwrap_or_default())
    }
}

const RUNTIME_RECORDS_TABLE: redb::TableDefinition<u64, &[u8]> =
    redb::TableDefinition::new("runtime_records");

#[derive(Debug, Clone)]
pub struct RedbTurnStore {
    db: Arc<Database>,
}

impl RedbTurnStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RuntimeError> {
        let path = path.as_ref();
        let db = Database::create(path)
            .map_err(|error| RuntimeError::Store(format!("failed to open redb store: {error}")))?;

        let write_txn = db.begin_write().map_err(|error| {
            RuntimeError::Store(format!("failed to begin redb initialization: {error}"))
        })?;
        {
            write_txn
                .open_table(RUNTIME_RECORDS_TABLE)
                .map_err(|error| {
                    RuntimeError::Store(format!("failed to open runtime records table: {error}"))
                })?;
        }
        write_txn.commit().map_err(|error| {
            RuntimeError::Store(format!("failed to commit redb initialization: {error}"))
        })?;

        Ok(Self { db: Arc::new(db) })
    }
}

#[async_trait]
impl RuntimeStore for RedbTurnStore {
    async fn append(&self, record: StoredTurnRecord) -> Result<(), RuntimeError> {
        let serialized = serde_json::to_vec(&record).map_err(|error| {
            RuntimeError::Store(format!("failed to serialize turn record: {error}"))
        })?;
        let write_txn = self
            .db
            .begin_write()
            .map_err(|error| RuntimeError::Store(format!("failed to begin redb write: {error}")))?;
        {
            let mut records_table =
                write_txn
                    .open_table(RUNTIME_RECORDS_TABLE)
                    .map_err(|error| {
                        RuntimeError::Store(format!(
                            "failed to open runtime records table: {error}"
                        ))
                    })?;
            let store_sequence = records_table
                .last()
                .map_err(|error| {
                    RuntimeError::Store(format!("failed to read last record: {error}"))
                })?
                .map(|(key, _)| key.value())
                .unwrap_or(0)
                + 1;
            records_table
                .insert(store_sequence, serialized.as_slice())
                .map_err(|error| {
                    RuntimeError::Store(format!("failed to insert record: {error}"))
                })?;
        }
        write_txn
            .commit()
            .map_err(|error| RuntimeError::Store(format!("failed to commit record: {error}")))?;
        Ok(())
    }

    async fn records(&self, session_id: &str) -> Result<Vec<StoredTurnRecord>, RuntimeError> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|error| RuntimeError::Store(format!("failed to begin redb read: {error}")))?;
        let records_table = read_txn
            .open_table(RUNTIME_RECORDS_TABLE)
            .map_err(|error| {
                RuntimeError::Store(format!("failed to open runtime records table: {error}"))
            })?;
        let mut records = Vec::new();

        for entry in records_table.iter().map_err(|error| {
            RuntimeError::Store(format!("failed to iterate runtime records: {error}"))
        })? {
            let (_, data) = entry
                .map_err(|error| RuntimeError::Store(format!("failed to read record: {error}")))?;
            let record: StoredTurnRecord =
                serde_json::from_slice(data.value()).map_err(|error| {
                    RuntimeError::Store(format!("failed to deserialize turn record: {error}"))
                })?;
            if record.session_id() == session_id {
                records.push(record);
            }
        }

        records.sort_by_key(|record| record.sequence());
        Ok(records)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeTrace {
    pub session_id: String,
    pub agent_id: String,
    pub records: Vec<StoredTurnRecord>,
    pub outbound_events: Vec<RuntimeEvent>,
}

pub async fn replay_session_trace<S>(
    store: &S,
    session_id: &str,
) -> Result<RuntimeTrace, RuntimeError>
where
    S: RuntimeStore,
{
    let records = store.records(session_id).await?;
    let agent_id = records
        .first()
        .map(|record| record.agent_id().to_string())
        .unwrap_or_default();
    Ok(build_trace(session_id, agent_id, records))
}

pub async fn replay_agent_session_trace<S>(
    store: &S,
    session_id: &str,
    agent_id: &str,
) -> Result<RuntimeTrace, RuntimeError>
where
    S: RuntimeStore,
{
    let records = store
        .records(session_id)
        .await?
        .into_iter()
        .filter(|record| record.agent_id() == agent_id)
        .collect();
    Ok(build_trace(session_id, agent_id.to_string(), records))
}

fn build_trace(session_id: &str, agent_id: String, records: Vec<StoredTurnRecord>) -> RuntimeTrace {
    let outbound_events = records
        .iter()
        .filter_map(|record| match record {
            StoredTurnRecord::AssistantMessage {
                session_id,
                agent_id,
                content,
                ..
            } => Some(RuntimeEvent::OutboundResponse {
                session_id: session_id.clone(),
                agent_id: agent_id.clone(),
                text: content.clone(),
            }),
            _ => None,
        })
        .collect();

    RuntimeTrace {
        session_id: session_id.to_string(),
        agent_id,
        records,
        outbound_events,
    }
}

pub struct AgentRuntime<P, S> {
    provider: P,
    store: S,
    tools: HashMap<String, Arc<dyn ToolExecutor>>,
    config: RuntimeConfig,
}

impl<P, S> AgentRuntime<P, S>
where
    P: AgentProvider,
    S: RuntimeStore,
{
    pub fn new(provider: P, store: S, config: RuntimeConfig) -> Self {
        Self {
            provider,
            store,
            tools: HashMap::new(),
            config,
        }
    }

    pub fn with_tool(mut self, name: impl Into<String>, tool: impl ToolExecutor) -> Self {
        self.tools.insert(name.into(), Arc::new(tool));
        self
    }

    pub async fn run_turn(&self, input: TurnInput) -> Result<TurnOutput, RuntimeError> {
        let mut sequence = self.next_sequence(&input.session_id).await?;
        self.store
            .append(StoredTurnRecord::UserMessage {
                record_id: new_record_id(),
                sequence,
                session_id: input.session_id.clone(),
                agent_id: input.agent_id.clone(),
                content: input.user_message.clone(),
            })
            .await?;
        sequence += 1;

        let messages = vec![ContextMessage {
            role: "user".to_string(),
            content: input.user_message.clone(),
        }];
        let mut tool_results = Vec::new();

        for _ in 0..self.config.max_tool_iterations {
            let response = self
                .provider
                .complete(ProviderRequest {
                    session_id: input.session_id.clone(),
                    agent_id: input.agent_id.clone(),
                    messages: messages.clone(),
                    tool_results: tool_results.clone(),
                })
                .await?;

            if let Some(final_text) = response.final_text {
                self.store
                    .append(StoredTurnRecord::AssistantMessage {
                        record_id: new_record_id(),
                        sequence,
                        session_id: input.session_id.clone(),
                        agent_id: input.agent_id.clone(),
                        content: final_text.clone(),
                    })
                    .await?;

                let event = RuntimeEvent::OutboundResponse {
                    session_id: input.session_id.clone(),
                    agent_id: input.agent_id.clone(),
                    text: final_text.clone(),
                };
                return Ok(TurnOutput {
                    session_id: input.session_id,
                    agent_id: input.agent_id,
                    final_text,
                    events: vec![event],
                    tool_results,
                });
            }

            for call in response.tool_calls {
                self.store
                    .append(StoredTurnRecord::AssistantToolCall {
                        record_id: new_record_id(),
                        sequence,
                        session_id: input.session_id.clone(),
                        agent_id: input.agent_id.clone(),
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    })
                    .await?;
                sequence += 1;

                let result = self.execute_tool(call).await;
                self.store
                    .append(StoredTurnRecord::ToolResult {
                        record_id: new_record_id(),
                        sequence,
                        session_id: input.session_id.clone(),
                        agent_id: input.agent_id.clone(),
                        call_id: result.call_id.clone(),
                        tool_name: result.tool_name.clone(),
                        content: result.content.clone(),
                        is_error: result.is_error,
                    })
                    .await?;
                sequence += 1;
                tool_results.push(result);
            }
        }

        Err(RuntimeError::IterationLimit(
            self.config.max_tool_iterations,
        ))
    }

    async fn execute_tool(&self, call: ToolCall) -> ToolResult {
        let call_id = call.id.clone();
        let tool_name = call.name.clone();
        let Some(tool) = self.tools.get(&call.name) else {
            return ToolResult {
                call_id,
                tool_name: tool_name.clone(),
                content: RuntimeError::ToolNotRegistered(tool_name).to_string(),
                is_error: true,
            };
        };

        match tool.execute(call).await {
            Ok(result) => result,
            Err(RuntimeError::Tool { message, .. }) => ToolResult {
                call_id,
                tool_name,
                content: message,
                is_error: true,
            },
            Err(error) => ToolResult {
                call_id,
                tool_name,
                content: error.to_string(),
                is_error: true,
            },
        }
    }

    async fn next_sequence(&self, session_id: &str) -> Result<u64, RuntimeError> {
        Ok(self.store.records(session_id).await?.len() as u64 + 1)
    }
}

fn new_record_id() -> String {
    ulid::Ulid::new().to_string()
}

/// Boundary adapters that let the runtime spine talk to Seidrum's existing
/// provider-router and tool-dispatch request/reply shapes without coupling the
/// runtime to concrete plugin processes or durable infrastructure.
pub mod boundaries {
    use async_trait::async_trait;
    use seidrum_common::events::{
        LlmCallConfig, LlmResponse, ToolCallRequest, ToolCallResponse, UnifiedLlmRequest,
        UnifiedMessage, UnifiedToolResult,
    };

    use crate::{
        AgentProvider, ProviderRequest, ProviderResponse, RuntimeError, ToolCall, ToolExecutor,
        ToolResult,
    };

    /// Async boundary for a provider-router shaped request/reply endpoint.
    #[async_trait]
    pub trait LlmRouterBoundary: Send + Sync + Clone + 'static {
        async fn complete_unified(&self, request: UnifiedLlmRequest)
            -> Result<LlmResponse, String>;
    }

    /// AgentProvider adapter for boundaries that accept UnifiedLlmRequest and
    /// return LlmResponse, matching the existing llm-router/provider contract.
    #[derive(Debug, Clone)]
    pub struct LlmRouterProvider<B> {
        boundary: B,
    }

    impl<B> LlmRouterProvider<B> {
        pub fn new(boundary: B) -> Self {
            Self { boundary }
        }
    }

    #[async_trait]
    impl<B> AgentProvider for LlmRouterProvider<B>
    where
        B: LlmRouterBoundary,
    {
        async fn complete(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, RuntimeError> {
            let response = self
                .boundary
                .complete_unified(provider_request_to_unified_llm_request(request))
                .await
                .map_err(|error| {
                    RuntimeError::Provider(format!("llm router boundary failed: {error}"))
                })?;

            ProviderResponse::try_from(response)
        }
    }

    /// Async boundary for a tool-dispatcher shaped request/reply endpoint.
    #[async_trait]
    pub trait ToolDispatchBoundary: Send + Sync + 'static {
        async fn call_tool(&self, request: ToolCallRequest) -> Result<ToolCallResponse, String>;
    }

    /// ToolExecutor adapter for boundaries that accept ToolCallRequest and
    /// return ToolCallResponse, matching the existing tool-dispatcher contract.
    #[derive(Debug, Clone)]
    pub struct ToolDispatchExecutor<B> {
        boundary: B,
    }

    impl<B> ToolDispatchExecutor<B> {
        pub fn new(boundary: B) -> Self {
            Self { boundary }
        }
    }

    #[async_trait]
    impl<B> ToolExecutor for ToolDispatchExecutor<B>
    where
        B: ToolDispatchBoundary,
    {
        async fn execute(&self, call: ToolCall) -> Result<ToolResult, RuntimeError> {
            let tool_name = call.name.clone();
            let call_id = call.id.clone();
            let response = self
                .boundary
                .call_tool(tool_call_to_dispatch_request(call))
                .await
                .map_err(|error| RuntimeError::Tool {
                    tool_name: tool_name.clone(),
                    message: format!("tool dispatch boundary failed: {error}"),
                })?;

            Ok(tool_dispatch_response_to_tool_result(
                call_id, tool_name, response,
            ))
        }
    }

    pub fn provider_request_to_unified_llm_request(request: ProviderRequest) -> UnifiedLlmRequest {
        let mut messages: Vec<UnifiedMessage> = request
            .messages
            .into_iter()
            .map(|message| UnifiedMessage {
                role: message.role,
                content: Some(message.content),
                tool_calls: None,
                tool_results: None,
            })
            .collect();

        messages.extend(
            request
                .tool_results
                .into_iter()
                .map(|result| UnifiedMessage {
                    role: "tool".to_string(),
                    content: Some(result.content.clone()),
                    tool_calls: None,
                    tool_results: Some(vec![UnifiedToolResult {
                        tool_call_id: result.call_id,
                        content: result.content,
                        is_error: result.is_error,
                    }]),
                }),
        );

        UnifiedLlmRequest {
            agent_id: request.agent_id,
            messages,
            system_prompt: None,
            tools: Vec::new(),
            config: LlmCallConfig {
                temperature: None,
                max_tokens: None,
                top_p: None,
            },
            routing_strategy: "runtime-boundary".to_string(),
            model_preferences: Vec::new(),
            correlation_id: Some(request.session_id),
            scope: None,
            user_id: None,
        }
    }

    pub fn tool_call_to_dispatch_request(call: ToolCall) -> ToolCallRequest {
        ToolCallRequest {
            tool_id: call.name,
            plugin_id: String::new(),
            arguments: call.arguments,
            correlation_id: Some(call.id),
        }
    }

    fn tool_dispatch_response_to_tool_result(
        call_id: String,
        tool_name: String,
        response: ToolCallResponse,
    ) -> ToolResult {
        ToolResult {
            call_id,
            tool_name,
            content: result_content_to_string(response.result),
            is_error: response.is_error,
        }
    }

    fn result_content_to_string(value: serde_json::Value) -> String {
        match value {
            serde_json::Value::String(text) => text,
            other => other.to_string(),
        }
    }

    impl TryFrom<LlmResponse> for ProviderResponse {
        type Error = RuntimeError;

        fn try_from(response: LlmResponse) -> Result<Self, Self::Error> {
            let tool_calls = response
                .tool_calls
                .unwrap_or_default()
                .into_iter()
                .map(|call| {
                    let arguments = serde_json::from_str(&call.arguments).map_err(|error| {
                        RuntimeError::Provider(format!(
                            "invalid tool arguments for call `{}`: {error}",
                            call.id
                        ))
                    })?;

                    Ok(ToolCall {
                        id: call.id,
                        name: call.function_name,
                        arguments,
                    })
                })
                .collect::<Result<Vec<_>, RuntimeError>>()?;

            Ok(ProviderResponse {
                final_text: response.content,
                tool_calls,
            })
        }
    }
}
