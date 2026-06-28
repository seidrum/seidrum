use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use seidrum_agent_runtime::{
    AgentProvider, AgentRuntime, InMemoryTurnStore, ProviderRequest, ProviderResponse,
    RuntimeConfig, RuntimeError, RuntimeEvent, RuntimeStore, StoredTurnRecord, ToolCall,
    ToolExecutor, ToolResult, TurnInput,
};
use serde_json::json;

#[derive(Clone)]
struct ScriptedProvider {
    responses: Arc<Mutex<VecDeque<ProviderResponse>>>,
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}

impl ScriptedProvider {
    fn new(responses: impl IntoIterator<Item = ProviderResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into_iter().collect())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn requests(&self) -> Vec<ProviderRequest> {
        self.requests
            .lock()
            .expect("requests lock poisoned")
            .clone()
    }
}

#[async_trait]
impl AgentProvider for ScriptedProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, RuntimeError> {
        self.requests
            .lock()
            .expect("requests lock poisoned")
            .push(request);
        self.responses
            .lock()
            .expect("responses lock poisoned")
            .pop_front()
            .ok_or_else(|| RuntimeError::Provider("no scripted response".to_string()))
    }
}

#[derive(Clone)]
struct EchoTool;

#[async_trait]
impl ToolExecutor for EchoTool {
    async fn execute(&self, call: ToolCall) -> Result<ToolResult, RuntimeError> {
        let value = call
            .arguments
            .get("text")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        Ok(ToolResult {
            call_id: call.id,
            tool_name: call.name,
            content: format!("echo: {value}"),
            is_error: false,
        })
    }
}

#[derive(Clone)]
struct FailingTool;

#[async_trait]
impl ToolExecutor for FailingTool {
    async fn execute(&self, call: ToolCall) -> Result<ToolResult, RuntimeError> {
        Err(RuntimeError::Tool {
            tool_name: call.name,
            message: "boom".to_string(),
        })
    }
}

fn input() -> TurnInput {
    TurnInput {
        session_id: "session-1".to_string(),
        agent_id: "agent-1".to_string(),
        user_message: "hello".to_string(),
    }
}

#[tokio::test]
async fn run_turn_without_tools_persists_user_and_assistant_and_returns_outbound_event() {
    let provider = ScriptedProvider::new([ProviderResponse::final_text("hi there")]);
    let store = InMemoryTurnStore::default();
    let runtime = AgentRuntime::new(provider, store.clone(), RuntimeConfig::default());

    let output = runtime.run_turn(input()).await.expect("turn should run");

    assert_eq!(output.session_id, "session-1");
    assert_eq!(output.agent_id, "agent-1");
    assert_eq!(output.final_text, "hi there");
    assert_eq!(output.events.len(), 1);
    assert_eq!(
        output.events[0],
        RuntimeEvent::OutboundResponse {
            session_id: "session-1".to_string(),
            agent_id: "agent-1".to_string(),
            text: "hi there".to_string(),
        }
    );

    let records = store
        .records("session-1")
        .await
        .expect("records should load");
    assert_eq!(records.len(), 2);
    assert!(matches!(records[0], StoredTurnRecord::UserMessage { .. }));
    assert!(matches!(
        records[1],
        StoredTurnRecord::AssistantMessage { .. }
    ));
}

#[tokio::test]
async fn run_turn_executes_one_registered_tool_then_persists_final_response() {
    let provider = ScriptedProvider::new([
        ProviderResponse::tool_calls([ToolCall {
            id: "call-1".to_string(),
            name: "echo".to_string(),
            arguments: json!({ "text": "ping" }),
        }]),
        ProviderResponse::final_text("tool says echo: ping"),
    ]);
    let store = InMemoryTurnStore::default();
    let runtime = AgentRuntime::new(provider.clone(), store.clone(), RuntimeConfig::default())
        .with_tool("echo", EchoTool);

    let output = runtime.run_turn(input()).await.expect("turn should run");

    assert_eq!(output.final_text, "tool says echo: ping");
    assert_eq!(output.tool_results.len(), 1);
    assert_eq!(output.tool_results[0].content, "echo: ping");
    assert!(!output.tool_results[0].is_error);

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].tool_results.len(), 1);
    assert_eq!(requests[1].tool_results[0].content, "echo: ping");

    let records = store
        .records("session-1")
        .await
        .expect("records should load");
    assert!(records.iter().any(|record| matches!(
        record,
        StoredTurnRecord::AssistantToolCall { tool_name, .. } if tool_name == "echo"
    )));
    assert!(records.iter().any(|record| matches!(
        record,
        StoredTurnRecord::ToolResult { tool_name, content, is_error, .. }
            if tool_name == "echo" && content == "echo: ping" && !is_error
    )));
    assert!(matches!(
        records.last(),
        Some(StoredTurnRecord::AssistantMessage { .. })
    ));
}

#[tokio::test]
async fn run_turn_records_tool_error_and_continues_provider_loop() {
    let provider = ScriptedProvider::new([
        ProviderResponse::tool_calls([ToolCall {
            id: "call-1".to_string(),
            name: "fail".to_string(),
            arguments: json!({}),
        }]),
        ProviderResponse::final_text("I could not run the tool"),
    ]);
    let store = InMemoryTurnStore::default();
    let runtime = AgentRuntime::new(provider.clone(), store.clone(), RuntimeConfig::default())
        .with_tool("fail", FailingTool);

    let output = runtime
        .run_turn(input())
        .await
        .expect("turn should recover");

    assert_eq!(output.final_text, "I could not run the tool");
    assert_eq!(output.tool_results.len(), 1);
    assert!(output.tool_results[0].is_error);
    assert_eq!(output.tool_results[0].content, "boom");

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].tool_results[0].is_error);

    let records = store
        .records("session-1")
        .await
        .expect("records should load");
    assert!(records.iter().any(|record| matches!(
        record,
        StoredTurnRecord::ToolResult { tool_name, content, is_error, .. }
            if tool_name == "fail" && content == "boom" && *is_error
    )));
}

#[tokio::test]
async fn persisted_trace_records_include_session_agent_and_ordered_record_ids() {
    let provider = ScriptedProvider::new([ProviderResponse::final_text("done")]);
    let store = InMemoryTurnStore::default();
    let runtime = AgentRuntime::new(provider, store.clone(), RuntimeConfig::default());

    runtime.run_turn(input()).await.expect("turn should run");

    let records = store
        .records("session-1")
        .await
        .expect("records should load");
    assert_eq!(records.len(), 2);
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.session_id(), "session-1");
        assert_eq!(record.agent_id(), "agent-1");
        assert_eq!(record.sequence(), index as u64 + 1);
        assert!(!record.record_id().is_empty());
    }
}
