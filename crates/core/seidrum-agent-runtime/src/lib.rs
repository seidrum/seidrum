use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
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
