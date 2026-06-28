use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use seidrum_agent_runtime::boundaries::{
    provider_request_to_unified_llm_request, tool_call_to_dispatch_request, LlmRouterBoundary,
    LlmRouterProvider, ToolDispatchBoundary, ToolDispatchExecutor,
};
use seidrum_agent_runtime::{
    AgentRuntime, InMemoryTurnStore, ProviderRequest, ProviderResponse, RuntimeConfig,
    RuntimeError, ToolCall, ToolResult, TurnInput,
};
use seidrum_common::events::{LlmResponse, TokenUsage, ToolCallResponse, UnifiedLlmRequest};
use serde_json::json;

#[derive(Clone)]
struct FakeLlmBoundary {
    requests: Arc<Mutex<Vec<UnifiedLlmRequest>>>,
    responses: Arc<Mutex<Vec<Result<LlmResponse, String>>>>,
}

impl FakeLlmBoundary {
    fn new(responses: Vec<Result<LlmResponse, String>>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(responses.into_iter().rev().collect())),
        }
    }

    fn requests(&self) -> Vec<UnifiedLlmRequest> {
        self.requests
            .lock()
            .expect("requests lock poisoned")
            .clone()
    }
}

#[async_trait]
impl LlmRouterBoundary for FakeLlmBoundary {
    async fn complete_unified(&self, request: UnifiedLlmRequest) -> Result<LlmResponse, String> {
        self.requests
            .lock()
            .expect("requests lock poisoned")
            .push(request);
        self.responses
            .lock()
            .expect("responses lock poisoned")
            .pop()
            .expect("scripted response available")
    }
}

#[derive(Clone)]
struct FakeToolBoundary {
    requests: Arc<Mutex<Vec<seidrum_common::events::ToolCallRequest>>>,
    responses: Arc<Mutex<Vec<Result<ToolCallResponse, String>>>>,
}

impl FakeToolBoundary {
    fn new(responses: Vec<Result<ToolCallResponse, String>>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(responses.into_iter().rev().collect())),
        }
    }

    fn requests(&self) -> Vec<seidrum_common::events::ToolCallRequest> {
        self.requests
            .lock()
            .expect("requests lock poisoned")
            .clone()
    }
}

#[async_trait]
impl ToolDispatchBoundary for FakeToolBoundary {
    async fn call_tool(
        &self,
        request: seidrum_common::events::ToolCallRequest,
    ) -> Result<ToolCallResponse, String> {
        self.requests
            .lock()
            .expect("requests lock poisoned")
            .push(request);
        self.responses
            .lock()
            .expect("responses lock poisoned")
            .pop()
            .expect("scripted response available")
    }
}

fn llm_response(
    content: Option<&str>,
    tool_calls: Option<Vec<seidrum_common::events::ToolCall>>,
) -> LlmResponse {
    LlmResponse {
        agent_id: "agent-1".to_string(),
        content: content.map(str::to_string),
        tool_calls,
        model_used: "fake-model".to_string(),
        provider: "fake-provider".to_string(),
        tokens: TokenUsage {
            prompt_tokens: 1,
            completion_tokens: 1,
            total_tokens: 2,
            estimated_cost_usd: 0.0,
        },
        duration_ms: 1,
        finish_reason: "stop".to_string(),
        tool_rounds: 0,
    }
}

fn input() -> TurnInput {
    TurnInput {
        session_id: "session-1".to_string(),
        agent_id: "agent-1".to_string(),
        user_message: "hello".to_string(),
    }
}

#[test]
fn provider_request_converts_to_unified_llm_router_shape_with_tool_results() {
    let request = ProviderRequest {
        session_id: "session-1".to_string(),
        agent_id: "agent-1".to_string(),
        messages: vec![seidrum_agent_runtime::ContextMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        }],
        tool_results: vec![ToolResult {
            call_id: "call-1".to_string(),
            tool_name: "echo".to_string(),
            content: "echo: ping".to_string(),
            is_error: false,
        }],
    };

    let unified = provider_request_to_unified_llm_request(request);

    assert_eq!(unified.agent_id, "agent-1");
    assert_eq!(unified.correlation_id.as_deref(), Some("session-1"));
    assert_eq!(unified.messages[0].role, "user");
    assert_eq!(unified.messages[0].content.as_deref(), Some("hello"));
    assert_eq!(unified.messages[1].role, "tool");
    let tool_results = unified.messages[1].tool_results.as_ref().unwrap();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0].tool_call_id, "call-1");
    assert_eq!(tool_results[0].content, "echo: ping");
    assert!(!tool_results[0].is_error);
}

#[test]
fn llm_router_response_converts_tool_calls_and_invalid_arguments_to_runtime_response() {
    let response = llm_response(
        None,
        Some(vec![seidrum_common::events::ToolCall {
            id: "call-1".to_string(),
            function_name: "echo".to_string(),
            arguments: "{\"text\":\"ping\"}".to_string(),
        }]),
    );

    let converted = ProviderResponse::try_from(response).expect("response converts");

    assert_eq!(converted.final_text, None);
    assert_eq!(converted.tool_calls.len(), 1);
    assert_eq!(converted.tool_calls[0].id, "call-1");
    assert_eq!(converted.tool_calls[0].name, "echo");
    assert_eq!(converted.tool_calls[0].arguments, json!({"text": "ping"}));

    let invalid = llm_response(
        None,
        Some(vec![seidrum_common::events::ToolCall {
            id: "call-2".to_string(),
            function_name: "bad".to_string(),
            arguments: "not json".to_string(),
        }]),
    );
    let error = ProviderResponse::try_from(invalid).expect_err("invalid JSON should fail");
    assert!(error.to_string().contains("invalid tool arguments"));
}

#[test]
fn tool_call_converts_to_tool_dispatch_request_reply_shape() {
    let request = tool_call_to_dispatch_request(ToolCall {
        id: "call-1".to_string(),
        name: "echo".to_string(),
        arguments: json!({"text": "ping"}),
    });

    assert_eq!(request.tool_id, "echo");
    assert_eq!(request.plugin_id, "");
    assert_eq!(request.arguments, json!({"text": "ping"}));
    assert_eq!(request.correlation_id.as_deref(), Some("call-1"));
}

#[tokio::test]
async fn run_turn_can_use_llm_router_and_tool_dispatch_boundaries() {
    let llm_boundary = FakeLlmBoundary::new(vec![
        Ok(llm_response(
            None,
            Some(vec![seidrum_common::events::ToolCall {
                id: "call-1".to_string(),
                function_name: "echo".to_string(),
                arguments: "{\"text\":\"ping\"}".to_string(),
            }]),
        )),
        Ok(llm_response(Some("tool says echo: ping"), None)),
    ]);
    let tool_boundary = FakeToolBoundary::new(vec![Ok(ToolCallResponse {
        tool_id: "echo".to_string(),
        result: json!("echo: ping"),
        is_error: false,
    })]);

    let runtime = AgentRuntime::new(
        LlmRouterProvider::new(llm_boundary.clone()),
        InMemoryTurnStore::default(),
        RuntimeConfig::default(),
    )
    .with_tool("echo", ToolDispatchExecutor::new(tool_boundary.clone()));

    let output = runtime.run_turn(input()).await.expect("turn should run");

    assert_eq!(output.final_text, "tool says echo: ping");
    assert_eq!(output.tool_results.len(), 1);
    assert_eq!(output.tool_results[0].content, "echo: ping");
    assert!(!output.tool_results[0].is_error);

    let llm_requests = llm_boundary.requests();
    assert_eq!(llm_requests.len(), 2);
    assert_eq!(llm_requests[1].messages[1].role, "tool");

    let tool_requests = tool_boundary.requests();
    assert_eq!(tool_requests.len(), 1);
    assert_eq!(tool_requests[0].tool_id, "echo");
    assert_eq!(tool_requests[0].correlation_id.as_deref(), Some("call-1"));
}

#[tokio::test]
async fn boundary_failures_are_reported_as_clear_runtime_errors_or_tool_results() {
    let provider = LlmRouterProvider::new(FakeLlmBoundary::new(vec![Err(
        "router unavailable".to_string()
    )]));
    let runtime = AgentRuntime::new(
        provider,
        InMemoryTurnStore::default(),
        RuntimeConfig::default(),
    );
    let provider_error = runtime
        .run_turn(input())
        .await
        .expect_err("provider should fail");
    assert_eq!(
        provider_error,
        RuntimeError::Provider("llm router boundary failed: router unavailable".to_string())
    );

    let llm_boundary = FakeLlmBoundary::new(vec![
        Ok(llm_response(
            None,
            Some(vec![seidrum_common::events::ToolCall {
                id: "call-1".to_string(),
                function_name: "echo".to_string(),
                arguments: "{}".to_string(),
            }]),
        )),
        Ok(llm_response(Some("saw tool failure"), None)),
    ]);
    let tool_boundary = FakeToolBoundary::new(vec![Err("dispatcher timeout".to_string())]);
    let runtime = AgentRuntime::new(
        LlmRouterProvider::new(llm_boundary),
        InMemoryTurnStore::default(),
        RuntimeConfig::default(),
    )
    .with_tool("echo", ToolDispatchExecutor::new(tool_boundary));

    let output = runtime
        .run_turn(input())
        .await
        .expect("turn should continue");
    assert_eq!(output.final_text, "saw tool failure");
    assert_eq!(output.tool_results[0].tool_name, "echo");
    assert!(output.tool_results[0].is_error);
    assert_eq!(
        output.tool_results[0].content,
        "tool dispatch boundary failed: dispatcher timeout"
    );
}
