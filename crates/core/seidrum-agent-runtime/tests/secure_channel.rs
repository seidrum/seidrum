use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use seidrum_agent_runtime::{
    AgentProvider, AgentRuntime, ChannelContinuationIdentity, ChannelKind, ChannelTurnInput,
    InMemoryTurnStore, ProviderRequest, ProviderResponse, RuntimeConfig, RuntimeStore,
    SecureToolExecutor, StoredTurnRecord, ToolAuthorizationBoundary, ToolAuthorizationRequest,
    ToolCall, ToolExecutor, ToolPermissionDecision, ToolResult,
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
    async fn complete(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, seidrum_agent_runtime::RuntimeError> {
        self.requests
            .lock()
            .expect("requests lock poisoned")
            .push(request);
        self.responses
            .lock()
            .expect("responses lock poisoned")
            .pop_front()
            .ok_or_else(|| {
                seidrum_agent_runtime::RuntimeError::Provider("no scripted response".to_string())
            })
    }
}

#[derive(Clone)]
struct RecordingTool {
    calls: Arc<Mutex<Vec<ToolCall>>>,
}

impl RecordingTool {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> Vec<ToolCall> {
        self.calls.lock().expect("calls lock poisoned").clone()
    }
}

#[async_trait]
impl ToolExecutor for RecordingTool {
    async fn execute(
        &self,
        call: ToolCall,
    ) -> Result<ToolResult, seidrum_agent_runtime::RuntimeError> {
        self.calls
            .lock()
            .expect("calls lock poisoned")
            .push(call.clone());
        Ok(ToolResult {
            call_id: call.id,
            tool_name: call.name,
            content: "tool ran".to_string(),
            is_error: false,
        })
    }
}

#[derive(Clone)]
struct StaticAuthorizationBoundary {
    decision: ToolPermissionDecision,
    requests: Arc<Mutex<Vec<ToolAuthorizationRequest>>>,
}

impl StaticAuthorizationBoundary {
    fn new(decision: ToolPermissionDecision) -> Self {
        Self {
            decision,
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn requests(&self) -> Vec<ToolAuthorizationRequest> {
        self.requests
            .lock()
            .expect("requests lock poisoned")
            .clone()
    }
}

#[async_trait]
impl ToolAuthorizationBoundary for StaticAuthorizationBoundary {
    async fn authorize(&self, request: ToolAuthorizationRequest) -> ToolPermissionDecision {
        self.requests
            .lock()
            .expect("requests lock poisoned")
            .push(request);
        self.decision.clone()
    }
}

fn telegram_origin(thread_id: Option<&str>) -> ChannelContinuationIdentity {
    ChannelContinuationIdentity {
        provider: ChannelKind::Telegram,
        chat_id: "chat-42".to_string(),
        thread_id: thread_id.map(str::to_string),
        user_id: Some("user-7".to_string()),
        message_id: Some("msg-99".to_string()),
        correlation_id: Some("corr-1".to_string()),
    }
}

#[test]
fn telegram_origin_maps_to_stable_session_with_thread() {
    let origin = telegram_origin(Some("topic-3"));
    let same_thread_different_message = ChannelContinuationIdentity {
        message_id: Some("msg-100".to_string()),
        correlation_id: Some("corr-2".to_string()),
        ..origin.clone()
    };
    let different_thread = telegram_origin(Some("topic-4"));
    let no_thread = telegram_origin(None);

    assert_eq!(
        origin.session_id(),
        same_thread_different_message.session_id()
    );
    assert_eq!(
        origin.session_id(),
        "channel:telegram:chat:chat-42:thread:topic-3"
    );
    assert_ne!(origin.session_id(), different_thread.session_id());
    assert_eq!(no_thread.session_id(), "channel:telegram:chat:chat-42");
}

#[tokio::test]
async fn channel_turn_runs_through_runtime_without_bypassing_boundaries() {
    let provider = ScriptedProvider::new([ProviderResponse::final_text("hello from channel")]);
    let store = InMemoryTurnStore::default();
    let runtime = AgentRuntime::new(provider.clone(), store.clone(), RuntimeConfig::default());
    let origin = telegram_origin(Some("topic-3"));

    let output = runtime
        .run_channel_turn(ChannelTurnInput {
            origin: origin.clone(),
            agent_id: "agent-1".to_string(),
            user_message: "hi".to_string(),
        })
        .await
        .expect("channel turn should run");

    assert_eq!(output.session_id, origin.session_id());
    assert_eq!(output.final_text, "hello from channel");
    assert_eq!(provider.requests()[0].session_id, origin.session_id());
    let records = store
        .records(&origin.session_id())
        .await
        .expect("records load");
    assert!(matches!(records[0], StoredTurnRecord::UserMessage { .. }));
}

#[tokio::test]
async fn secure_tool_executor_allows_authorized_tool() {
    let tool = RecordingTool::new();
    let boundary = StaticAuthorizationBoundary::new(ToolPermissionDecision::Allow);
    let executor = SecureToolExecutor::new(tool.clone(), boundary.clone())
        .with_session_id("session-1")
        .with_channel_origin(telegram_origin(Some("topic-3")));

    let result = executor
        .execute(ToolCall {
            id: "call-1".to_string(),
            name: "echo".to_string(),
            arguments: json!({"text": "ping"}),
        })
        .await
        .expect("authorized tool runs");

    assert_eq!(result.content, "tool ran");
    assert!(!result.is_error);
    assert_eq!(tool.calls().len(), 1);
    let requests = boundary.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].session_id.as_deref(), Some("session-1"));
    assert_eq!(requests[0].tool_name, "echo");
    assert_eq!(
        requests[0].channel_origin.as_ref().unwrap().session_id(),
        "channel:telegram:chat:chat-42:thread:topic-3"
    );
}

#[tokio::test]
async fn secure_tool_executor_records_denied_tool_as_error_result() {
    let tool = RecordingTool::new();
    let boundary = StaticAuthorizationBoundary::new(ToolPermissionDecision::Deny {
        reason: "not allowed in this channel".to_string(),
    });
    let executor = SecureToolExecutor::new(tool.clone(), boundary.clone());

    let result = executor
        .execute(ToolCall {
            id: "call-1".to_string(),
            name: "dangerous".to_string(),
            arguments: json!({}),
        })
        .await
        .expect("denial is a tool error result, not a runtime abort");

    assert!(result.is_error);
    assert_eq!(result.tool_name, "dangerous");
    assert!(result.content.contains("tool authorization denied"));
    assert!(result.content.contains("not allowed in this channel"));
    assert!(tool.calls().is_empty());
    assert_eq!(boundary.requests().len(), 1);
}

#[tokio::test]
async fn approval_required_tool_decision_is_explicit_and_non_executing() {
    let tool = RecordingTool::new();
    let boundary = StaticAuthorizationBoundary::new(ToolPermissionDecision::RequireApproval {
        reason: "requires human approval".to_string(),
    });
    let executor = SecureToolExecutor::new(tool.clone(), boundary.clone());

    let result = executor
        .execute(ToolCall {
            id: "call-2".to_string(),
            name: "send_notification".to_string(),
            arguments: json!({"text": "hi"}),
        })
        .await
        .expect("approval requirement is surfaced as tool error result");

    assert!(result.is_error);
    assert_eq!(result.tool_name, "send_notification");
    assert!(result
        .content
        .contains("tool authorization requires approval"));
    assert!(result.content.contains("requires human approval"));
    assert!(tool.calls().is_empty());
    assert_eq!(boundary.requests().len(), 1);
}
