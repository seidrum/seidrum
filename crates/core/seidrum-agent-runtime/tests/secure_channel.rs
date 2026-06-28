use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use seidrum_agent_runtime::{
    AgentProvider, AgentRuntime, AuditedSecureToolExecutor, AuthorizationAuditStore,
    ChannelContinuationIdentity, ChannelKind, ChannelTurnInput, InMemoryAuthorizationAuditStore,
    InMemoryTurnStore, ProviderRequest, ProviderResponse, RuleBasedToolAuthorizationBoundary,
    RuntimeConfig, RuntimeStore, SecureToolExecutor, StoredTurnRecord, TelegramInboundMessage,
    ToolAuthorizationBoundary, ToolAuthorizationContext, ToolAuthorizationRequest,
    ToolAuthorizationRule, ToolCall, ToolExecutor, ToolPermissionDecision, ToolResult,
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

#[tokio::test]
async fn audited_secure_tool_executor_records_allowed_decision_with_channel_context() {
    let tool = RecordingTool::new();
    let boundary = StaticAuthorizationBoundary::new(ToolPermissionDecision::Allow);
    let audit_store = InMemoryAuthorizationAuditStore::default();
    let origin = telegram_origin(Some("topic-3"));
    let executor = AuditedSecureToolExecutor::new(tool.clone(), boundary, audit_store.clone())
        .with_session_id("session-1")
        .with_channel_origin(origin.clone());

    let result = executor
        .execute(ToolCall {
            id: "call-allow".to_string(),
            name: "echo".to_string(),
            arguments: json!({"text": "ping"}),
        })
        .await
        .expect("allowed tool should execute");

    assert_eq!(result.content, "tool ran");
    assert_eq!(tool.calls().len(), 1);
    let audits = audit_store
        .authorization_audits_for_session("session-1")
        .await
        .expect("audits should list");
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].decision, ToolPermissionDecision::Allow);
    assert_eq!(audits[0].tool_name, "echo");
    assert_eq!(audits[0].call_id, "call-allow");
    assert_eq!(audits[0].channel_origin.as_ref(), Some(&origin));
}

#[tokio::test]
async fn audited_secure_tool_executor_records_denied_and_approval_without_inner_execution() {
    let tool = RecordingTool::new();
    let audit_store = InMemoryAuthorizationAuditStore::default();
    let denied = AuditedSecureToolExecutor::new(
        tool.clone(),
        StaticAuthorizationBoundary::new(ToolPermissionDecision::Deny {
            reason: "nope".to_string(),
        }),
        audit_store.clone(),
    )
    .with_session_id("session-1");

    let deny_result = denied
        .execute(ToolCall {
            id: "call-deny".to_string(),
            name: "dangerous".to_string(),
            arguments: json!({}),
        })
        .await
        .expect("denial should return tool error");
    assert!(deny_result.is_error);
    assert!(deny_result.content.contains("tool authorization denied"));

    let approval = AuditedSecureToolExecutor::new(
        tool.clone(),
        StaticAuthorizationBoundary::new(ToolPermissionDecision::RequireApproval {
            reason: "ask first".to_string(),
        }),
        audit_store.clone(),
    )
    .with_session_id("session-1");
    let approval_result = approval
        .execute(ToolCall {
            id: "call-approval".to_string(),
            name: "send_notification".to_string(),
            arguments: json!({"text": "hi"}),
        })
        .await
        .expect("approval requirement should return tool error");
    assert!(approval_result.is_error);
    assert!(approval_result
        .content
        .contains("tool authorization requires approval"));
    assert!(tool.calls().is_empty());

    let audits = audit_store
        .authorization_audits_for_session("session-1")
        .await
        .expect("audits should list");
    assert_eq!(audits.len(), 2);
    assert_eq!(audits[0].tool_name, "dangerous");
    assert_eq!(
        audits[0].decision,
        ToolPermissionDecision::Deny {
            reason: "nope".to_string()
        }
    );
    assert_eq!(audits[0].reason.as_deref(), Some("nope"));
    assert_eq!(audits[1].tool_name, "send_notification");
    assert_eq!(
        audits[1].decision,
        ToolPermissionDecision::RequireApproval {
            reason: "ask first".to_string()
        }
    );
    assert_eq!(audits[1].reason.as_deref(), Some("ask first"));
}

#[tokio::test]
async fn rule_based_authorization_policy_applies_tool_decisions() {
    let policy = RuleBasedToolAuthorizationBoundary::default_deny([
        ToolAuthorizationRule::for_tool("echo", ToolPermissionDecision::Allow),
        ToolAuthorizationRule::for_tool(
            "dangerous",
            ToolPermissionDecision::Deny {
                reason: "dangerous disabled".to_string(),
            },
        ),
        ToolAuthorizationRule::for_tool(
            "send_notification",
            ToolPermissionDecision::RequireApproval {
                reason: "outbound delivery needs approval".to_string(),
            },
        )
        .with_channel_kind(ChannelKind::Telegram),
    ]);

    let context = ToolAuthorizationContext {
        session_id: Some("session-1".to_string()),
        channel_origin: Some(telegram_origin(Some("topic-3"))),
    };

    assert_eq!(
        policy.decision_for("echo", context.clone()).await,
        ToolPermissionDecision::Allow
    );
    assert_eq!(
        policy.decision_for("dangerous", context.clone()).await,
        ToolPermissionDecision::Deny {
            reason: "dangerous disabled".to_string()
        }
    );
    assert_eq!(
        policy
            .decision_for("send_notification", context.clone())
            .await,
        ToolPermissionDecision::RequireApproval {
            reason: "outbound delivery needs approval".to_string()
        }
    );
    assert_eq!(
        policy.decision_for("unknown", context).await,
        ToolPermissionDecision::Deny {
            reason: "no matching authorization rule".to_string()
        }
    );
}

#[test]
fn telegram_inbound_maps_to_channel_turn_without_network_or_secrets() {
    let first = TelegramInboundMessage {
        chat_id: "chat-42".to_string(),
        thread_id: Some("topic-3".to_string()),
        user_id: Some("user-7".to_string()),
        message_id: "msg-99".to_string(),
        correlation_id: Some("telegram:chat-42:msg-99".to_string()),
        text: "hello".to_string(),
    };
    let second_same_thread = TelegramInboundMessage {
        message_id: "msg-100".to_string(),
        correlation_id: Some("telegram:chat-42:msg-100".to_string()),
        text: "follow up".to_string(),
        ..first.clone()
    };
    let different_thread = TelegramInboundMessage {
        thread_id: Some("topic-4".to_string()),
        ..first.clone()
    };

    let turn = first.clone().into_channel_turn("agent-1");
    let same_thread_turn = second_same_thread.into_channel_turn("agent-1");
    let other_thread_turn = different_thread.into_channel_turn("agent-1");

    assert_eq!(turn.agent_id, "agent-1");
    assert_eq!(turn.user_message, "hello");
    assert_eq!(turn.origin.provider, ChannelKind::Telegram);
    assert_eq!(turn.origin.message_id.as_deref(), Some("msg-99"));
    assert_eq!(
        turn.origin.session_id(),
        "channel:telegram:chat:chat-42:thread:topic-3"
    );
    assert_eq!(
        turn.origin.session_id(),
        same_thread_turn.origin.session_id()
    );
    assert_ne!(
        turn.origin.session_id(),
        other_thread_turn.origin.session_id()
    );
}
