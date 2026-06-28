use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use seidrum_agent_runtime::{
    append_inspection_authorization_audit, enqueue_outbound_response, inspect_runtime_session,
    AgentProvider, AgentRuntime, AuthorizationAuditRecord, ChannelContinuationIdentity,
    ChannelKind, FakeOutboundDeliveryExecutor, FakeOutboundDeliverySink, OutboundDeliveryRecord,
    OutboundDeliveryStatus, OutboundDeliveryStore, OutboundRetryPolicy, ProviderRequest,
    ProviderResponse, RedbTurnStore, RuntimeConfig, RuntimeError, RuntimeEvent,
    ToolPermissionDecision,
};

#[derive(Clone)]
struct ScriptedProvider {
    responses: Arc<Mutex<VecDeque<ProviderResponse>>>,
}

impl ScriptedProvider {
    fn new(responses: impl IntoIterator<Item = ProviderResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into_iter().collect())),
        }
    }
}

#[async_trait]
impl AgentProvider for ScriptedProvider {
    async fn complete(&self, _request: ProviderRequest) -> Result<ProviderResponse, RuntimeError> {
        self.responses
            .lock()
            .expect("responses lock poisoned")
            .pop_front()
            .ok_or_else(|| RuntimeError::Provider("no scripted response".to_string()))
    }
}

fn telegram_origin() -> ChannelContinuationIdentity {
    ChannelContinuationIdentity {
        provider: ChannelKind::Telegram,
        chat_id: "chat-42".to_string(),
        thread_id: Some("topic-7".to_string()),
        user_id: Some("user-9".to_string()),
        message_id: Some("msg-1".to_string()),
        correlation_id: Some("corr-1".to_string()),
    }
}

#[tokio::test]
async fn outbound_delivery_records_persist_across_reopen() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join("runtime.redb");
    let origin = telegram_origin();

    let store = RedbTurnStore::open(&path).expect("store should open");
    store
        .put_outbound_delivery(OutboundDeliveryRecord::queued(
            "delivery-1",
            "session-1",
            Some(origin.clone()),
            "hello telegram",
            Some("trace-1"),
        ))
        .await
        .expect("delivery should persist");
    drop(store);

    let reopened = RedbTurnStore::open(&path).expect("store should reopen");
    let records = reopened
        .outbound_deliveries_for_session("session-1", None)
        .await
        .expect("delivery records should load after reopen");

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].delivery_id, "delivery-1");
    assert_eq!(records[0].session_id, "session-1");
    assert_eq!(records[0].channel_origin.as_ref(), Some(&origin));
    assert_eq!(records[0].payload_text, "hello telegram");
    assert_eq!(records[0].status, OutboundDeliveryStatus::Queued);
    assert_eq!(records[0].attempt_count, 0);
    assert_eq!(records[0].max_attempts, 3);
    assert_eq!(
        records[0].linked_runtime_event_id.as_deref(),
        Some("trace-1")
    );
}

#[tokio::test]
async fn channel_outbound_response_enqueues_delivery_without_network() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join("runtime.redb");
    let store = RedbTurnStore::open(&path).expect("store should open");
    let origin = telegram_origin();
    let runtime = AgentRuntime::new(
        ScriptedProvider::new([ProviderResponse::final_text("delivery text")]),
        store.clone(),
        RuntimeConfig::default(),
    );

    let output = runtime
        .run_channel_turn(seidrum_agent_runtime::ChannelTurnInput {
            origin: origin.clone(),
            agent_id: "agent-1".to_string(),
            user_message: "hello".to_string(),
        })
        .await
        .expect("channel turn should run");
    let delivery = enqueue_outbound_response(&store, &output.events[0], Some(origin.clone()))
        .await
        .expect("outbound response should enqueue without network");

    let queued = store
        .outbound_deliveries_for_session(&origin.session_id(), Some(OutboundDeliveryStatus::Queued))
        .await
        .expect("queued deliveries should load");
    assert_eq!(delivery.payload_text, "delivery text");
    assert_eq!(delivery.channel_origin.as_ref(), Some(&origin));
    assert_eq!(queued, vec![delivery]);
}

#[tokio::test]
async fn delivery_retry_policy_marks_retry_then_failed_after_max_attempts() {
    let mut delivery = OutboundDeliveryRecord::queued(
        "delivery-1",
        "session-1",
        Some(telegram_origin()),
        "payload",
        None::<String>,
    )
    .with_max_attempts(2);
    let policy = OutboundRetryPolicy::new(2, [10, 20]);

    policy
        .record_failure(&mut delivery, "temporary outage")
        .expect("first failure should schedule retry");
    assert_eq!(delivery.status, OutboundDeliveryStatus::RetryScheduled);
    assert_eq!(delivery.attempt_count, 1);
    assert_eq!(delivery.retry_after.as_deref(), Some("10"));
    assert_eq!(delivery.last_error.as_deref(), Some("temporary outage"));

    policy
        .record_failure(&mut delivery, "still down")
        .expect("second failure should exhaust attempts");
    assert_eq!(delivery.status, OutboundDeliveryStatus::Failed);
    assert_eq!(delivery.attempt_count, 2);
    assert_eq!(delivery.retry_after, None);
    assert_eq!(delivery.last_error.as_deref(), Some("still down"));
}

#[tokio::test]
async fn fake_delivery_executor_marks_sent_without_network() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join("runtime.redb");
    let store = RedbTurnStore::open(&path).expect("store should open");
    store
        .put_outbound_delivery(OutboundDeliveryRecord::queued(
            "delivery-1",
            "session-1",
            Some(telegram_origin()),
            "payload",
            None::<String>,
        ))
        .await
        .expect("delivery should persist");

    let executor = FakeOutboundDeliveryExecutor::new(
        store.clone(),
        FakeOutboundDeliverySink::successful("fake-message-1"),
        OutboundRetryPolicy::new(3, [1, 2, 3]),
    );
    let updated = executor
        .dispatch_one("delivery-1")
        .await
        .expect("fake dispatch should update delivery");

    assert_eq!(updated.status, OutboundDeliveryStatus::Sent);
    assert_eq!(updated.attempt_count, 1);
    assert_eq!(updated.last_error, None);
    assert_eq!(
        updated.provider_message_id.as_deref(),
        Some("fake-message-1")
    );
}

#[tokio::test]
async fn inspection_loads_trace_audit_and_delivery_for_session() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join("runtime.redb");
    let store = RedbTurnStore::open(&path).expect("store should open");
    let origin = telegram_origin();
    let session_id = origin.session_id();
    let runtime = AgentRuntime::new(
        ScriptedProvider::new([ProviderResponse::final_text("inspect me")]),
        store.clone(),
        RuntimeConfig::default(),
    );

    let output = runtime
        .run_channel_turn(seidrum_agent_runtime::ChannelTurnInput {
            origin: origin.clone(),
            agent_id: "agent-1".to_string(),
            user_message: "hello".to_string(),
        })
        .await
        .expect("channel turn should run");
    enqueue_outbound_response(&store, &output.events[0], Some(origin.clone()))
        .await
        .expect("delivery should enqueue");
    append_inspection_authorization_audit(
        &store,
        AuthorizationAuditRecord {
            audit_id: "audit-1".to_string(),
            sequence: 1,
            decision: ToolPermissionDecision::Allow,
            tool_name: "safe-tool".to_string(),
            call_id: "call-1".to_string(),
            session_id: Some(session_id.clone()),
            channel_origin: Some(origin),
            reason: None,
        },
    )
    .await
    .expect("audit should persist");

    let inspection = inspect_runtime_session(&store, &session_id)
        .await
        .expect("inspection should load without boundaries");

    assert_eq!(inspection.session_id, session_id);
    assert_eq!(inspection.trace.records.len(), 2);
    assert_eq!(
        inspection.trace.outbound_events,
        vec![RuntimeEvent::OutboundResponse {
            session_id: session_id.clone(),
            agent_id: "agent-1".to_string(),
            text: "inspect me".to_string(),
        }]
    );
    assert_eq!(inspection.authorization_audits.len(), 1);
    assert_eq!(inspection.outbound_deliveries.len(), 1);
    assert_eq!(inspection.outbound_deliveries[0].payload_text, "inspect me");
}
