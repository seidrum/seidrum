use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use seidrum_agent_runtime::{
    replay_agent_session_trace, replay_session_trace, AgentProvider, AgentRuntime,
    AuthorizationAuditRecord, AuthorizationAuditStore, ChannelContinuationIdentity, ChannelKind,
    ProviderRequest, ProviderResponse, RedbTurnStore, RuntimeConfig, RuntimeError, RuntimeEvent,
    RuntimeStore, StoredTurnRecord, ToolCall, ToolExecutor, ToolPermissionDecision, ToolResult,
    TurnInput,
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
struct PanicProvider;

#[async_trait]
impl AgentProvider for PanicProvider {
    async fn complete(&self, _request: ProviderRequest) -> Result<ProviderResponse, RuntimeError> {
        panic!("replay must not call provider boundaries")
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

fn input(session_id: &str, agent_id: &str, user_message: &str) -> TurnInput {
    TurnInput {
        session_id: session_id.to_string(),
        agent_id: agent_id.to_string(),
        user_message: user_message.to_string(),
    }
}

#[tokio::test]
async fn durable_store_persists_records_across_reopen() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join("runtime.redb");

    let store = RedbTurnStore::open(&path).expect("store should open");
    store
        .append(StoredTurnRecord::UserMessage {
            record_id: "record-1".to_string(),
            sequence: 1,
            session_id: "session-1".to_string(),
            agent_id: "agent-1".to_string(),
            content: "hello".to_string(),
        })
        .await
        .expect("append should persist");
    drop(store);

    let reopened = RedbTurnStore::open(&path).expect("store should reopen");
    let records = reopened
        .records("session-1")
        .await
        .expect("records should load after reopen");

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].record_id(), "record-1");
    assert_eq!(records[0].sequence(), 1);
}

#[tokio::test]
async fn run_turn_with_durable_store_can_be_replayed_without_provider_or_tool() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join("runtime.redb");
    let store = RedbTurnStore::open(&path).expect("store should open");
    let provider = ScriptedProvider::new([
        ProviderResponse::tool_calls([ToolCall {
            id: "call-1".to_string(),
            name: "echo".to_string(),
            arguments: json!({ "text": "ping" }),
        }]),
        ProviderResponse::final_text("tool says echo: ping"),
    ]);
    let runtime = AgentRuntime::new(provider.clone(), store.clone(), RuntimeConfig::default())
        .with_tool("echo", EchoTool);

    runtime
        .run_turn(input("session-1", "agent-1", "hello"))
        .await
        .expect("turn should run");
    assert_eq!(provider.requests().len(), 2);
    drop(runtime);
    drop(store);

    let replay_store = RedbTurnStore::open(&path).expect("store should reopen");
    let _unused_runtime = AgentRuntime::new(
        PanicProvider,
        replay_store.clone(),
        RuntimeConfig::default(),
    );
    let trace = replay_agent_session_trace(&replay_store, "session-1", "agent-1")
        .await
        .expect("trace should replay");

    assert_eq!(trace.session_id, "session-1");
    assert_eq!(trace.agent_id, "agent-1");
    assert_eq!(trace.records.len(), 4);
    assert!(matches!(
        trace.records[0],
        StoredTurnRecord::UserMessage { .. }
    ));
    assert!(matches!(
        trace.records[1],
        StoredTurnRecord::AssistantToolCall { .. }
    ));
    assert!(matches!(
        trace.records[2],
        StoredTurnRecord::ToolResult { .. }
    ));
    assert!(matches!(
        trace.records[3],
        StoredTurnRecord::AssistantMessage { .. }
    ));
    assert_eq!(
        trace.outbound_events,
        vec![RuntimeEvent::OutboundResponse {
            session_id: "session-1".to_string(),
            agent_id: "agent-1".to_string(),
            text: "tool says echo: ping".to_string(),
        }]
    );
}

#[tokio::test]
async fn replay_filters_by_session_and_preserves_order() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join("runtime.redb");
    let store = RedbTurnStore::open(&path).expect("store should open");

    for (session_id, sequence, content) in [
        ("session-1", 2, "second"),
        ("session-2", 1, "other"),
        ("session-1", 1, "first"),
    ] {
        store
            .append(StoredTurnRecord::UserMessage {
                record_id: format!("{session_id}-{sequence}"),
                sequence,
                session_id: session_id.to_string(),
                agent_id: "agent-1".to_string(),
                content: content.to_string(),
            })
            .await
            .expect("append should persist");
    }

    let trace = replay_session_trace(&store, "session-1")
        .await
        .expect("trace should replay");

    assert_eq!(trace.records.len(), 2);
    assert_eq!(trace.records[0].sequence(), 1);
    assert_eq!(trace.records[1].sequence(), 2);
    assert!(trace
        .records
        .iter()
        .all(|record| record.session_id() == "session-1"));
}

#[tokio::test]
async fn durable_store_reports_corrupt_or_unreadable_data_as_store_error() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join("runtime.redb");
    std::fs::write(&path, b"not a redb database").expect("write corrupt data");

    let error = RedbTurnStore::open(&path).expect_err("corrupt data should not open");

    assert!(matches!(error, RuntimeError::Store(message) if message.contains("redb")));
}

#[tokio::test]
async fn authorization_audit_records_persist_across_reopen() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join("runtime.redb");
    let origin = ChannelContinuationIdentity {
        provider: ChannelKind::Telegram,
        chat_id: "chat-42".to_string(),
        thread_id: Some("topic-3".to_string()),
        user_id: Some("user-7".to_string()),
        message_id: Some("msg-99".to_string()),
        correlation_id: Some("telegram:chat-42:msg-99".to_string()),
    };

    let store = RedbTurnStore::open(&path).expect("store should open");
    store
        .append_authorization_audit(AuthorizationAuditRecord {
            audit_id: "audit-1".to_string(),
            sequence: 1,
            decision: ToolPermissionDecision::Deny {
                reason: "blocked by test policy".to_string(),
            },
            tool_name: "dangerous".to_string(),
            call_id: "call-1".to_string(),
            session_id: Some("session-1".to_string()),
            channel_origin: Some(origin.clone()),
            reason: Some("blocked by test policy".to_string()),
        })
        .await
        .expect("audit record should persist");
    drop(store);

    let reopened = RedbTurnStore::open(&path).expect("store should reopen");
    let by_session = reopened
        .authorization_audits_for_session("session-1")
        .await
        .expect("audit records should load by session");
    let by_tool = reopened
        .authorization_audits_for_tool("dangerous")
        .await
        .expect("audit records should load by tool");

    assert_eq!(by_session.len(), 1);
    assert_eq!(by_tool, by_session);
    assert_eq!(by_session[0].audit_id, "audit-1");
    assert_eq!(by_session[0].sequence, 1);
    assert_eq!(by_session[0].session_id.as_deref(), Some("session-1"));
    assert_eq!(by_session[0].channel_origin.as_ref(), Some(&origin));
}
