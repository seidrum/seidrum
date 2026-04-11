//! E2E tests for consciousness built-in capabilities.
//!
//! Requires: NATS + kernel running with consciousness service.

mod common;

use seidrum_common::events::{ToolCallRequest, ToolCallResponse};

/// Call a built-in capability via capability.call.consciousness
async fn call_builtin(
    nats: &seidrum_common::bus_client::BusClient,
    tool_id: &str,
    arguments: serde_json::Value,
) -> ToolCallResponse {
    let req = ToolCallRequest {
        tool_id: tool_id.into(),
        plugin_id: "e2e-test".into(),
        arguments,
        correlation_id: None,
    };
    common::nats_request(nats, "capability.call.consciousness", &req).await
}

#[tokio::test]
#[ignore]
async fn test_builtin_search_skills() {
    let nats = common::connect_nats().await;

    let resp = call_builtin(
        &nats,
        "search-skills",
        serde_json::json!({"query": "code review", "limit": 5}),
    )
    .await;

    assert!(
        !resp.is_error,
        "search-skills should not error: {:?}",
        resp.result
    );
    assert_eq!(resp.tool_id, "search-skills");
    // Result should have a "skills" array
    assert!(resp.result.get("skills").is_some());
}

#[tokio::test]
#[ignore]
async fn test_builtin_save_and_load_skill() {
    let nats = common::connect_nats().await;
    let skill_desc = format!("E2E test skill {}", ulid::Ulid::new());

    // Save
    let save_resp = call_builtin(
        &nats,
        "save-skill",
        serde_json::json!({
            "description": skill_desc,
            "snippet": "This is a test snippet from E2E tests.",
            "tags": ["e2e"]
        }),
    )
    .await;
    assert!(
        !save_resp.is_error,
        "save-skill failed: {:?}",
        save_resp.result
    );
    let skill_id = save_resp.result.get("skill_id").unwrap().as_str().unwrap();

    // Load
    let load_resp = call_builtin(
        &nats,
        "load-skill",
        serde_json::json!({"skill_id": skill_id}),
    )
    .await;
    assert!(
        !load_resp.is_error,
        "load-skill failed: {:?}",
        load_resp.result
    );
    assert!(load_resp
        .result
        .get("snippet")
        .unwrap()
        .as_str()
        .unwrap()
        .contains("test snippet"));

    // Load nonexistent
    let bad_resp = call_builtin(
        &nats,
        "load-skill",
        serde_json::json!({"skill_id": "nonexistent-skill-xyz"}),
    )
    .await;
    assert!(bad_resp.is_error, "load-skill should error for nonexistent");
}

#[tokio::test]
#[ignore]
async fn test_builtin_subscribe_events_denied_subjects() {
    let nats = common::connect_nats().await;

    // Try subscribing to internal subject — should be denied
    let resp = call_builtin(
        &nats,
        "subscribe-events",
        serde_json::json!({"subjects": ["brain.query.request"]}),
    )
    .await;
    assert!(
        resp.is_error,
        "Should deny subscription to brain.* subjects"
    );
    assert!(resp
        .result
        .get("error")
        .unwrap()
        .as_str()
        .unwrap()
        .contains("Cannot subscribe to internal subject"));
}

#[tokio::test]
#[ignore]
async fn test_builtin_schedule_wake_bounds() {
    let nats = common::connect_nats().await;

    // Valid wake
    let resp = call_builtin(
        &nats,
        "schedule-wake",
        serde_json::json!({
            "delay_seconds": 60,
            "reason": "E2E test wake"
        }),
    )
    .await;
    assert!(
        !resp.is_error,
        "Valid schedule-wake should succeed: {:?}",
        resp.result
    );
    assert!(resp.result.get("scheduled").unwrap().as_bool().unwrap());

    // Invalid: 0 seconds
    let resp_zero = call_builtin(
        &nats,
        "schedule-wake",
        serde_json::json!({
            "delay_seconds": 0,
            "reason": "invalid"
        }),
    )
    .await;
    assert!(resp_zero.is_error, "delay_seconds=0 should be rejected");

    // Invalid: too large
    let resp_huge = call_builtin(
        &nats,
        "schedule-wake",
        serde_json::json!({
            "delay_seconds": 999999999,
            "reason": "invalid"
        }),
    )
    .await;
    assert!(resp_huge.is_error, "Huge delay should be rejected");
}

#[tokio::test]
#[ignore]
async fn test_builtin_list_conversations() {
    let nats = common::connect_nats().await;

    let resp = call_builtin(&nats, "list-conversations", serde_json::json!({"limit": 5})).await;
    assert!(
        !resp.is_error,
        "list-conversations should not error: {:?}",
        resp.result
    );
}

#[tokio::test]
#[ignore]
async fn test_builtin_invalid_arguments() {
    let nats = common::connect_nats().await;

    // save-skill with empty description
    let resp = call_builtin(
        &nats,
        "save-skill",
        serde_json::json!({"description": "", "snippet": ""}),
    )
    .await;
    assert!(resp.is_error, "Empty description should be rejected");

    // load-skill with empty skill_id
    let resp2 = call_builtin(&nats, "load-skill", serde_json::json!({"skill_id": ""})).await;
    assert!(resp2.is_error, "Empty skill_id should be rejected");
}

#[tokio::test]
#[ignore]
async fn test_builtin_delegate_task_depth_limit() {
    let nats = common::connect_nats().await;

    // Delegation at depth 5 should be rejected
    let resp = call_builtin(
        &nats,
        "delegate-task",
        serde_json::json!({
            "to_agent_id": "research-agent",
            "message": "test",
            "context": {"delegation_depth": 5}
        }),
    )
    .await;
    assert!(resp.is_error, "Delegation at depth 5 should be rejected");
    assert!(resp
        .result
        .get("error")
        .unwrap()
        .as_str()
        .unwrap()
        .contains("depth"));

    // Delegation at depth 0 should succeed (creates conversation)
    let resp2 = call_builtin(
        &nats,
        "delegate-task",
        serde_json::json!({
            "to_agent_id": "research-agent",
            "message": "E2E test delegation",
            "context": {"delegation_depth": 0}
        }),
    )
    .await;
    assert!(
        !resp2.is_error,
        "Delegation at depth 0 should succeed: {:?}",
        resp2.result
    );
    assert!(resp2.result.get("conversation_id").is_some());
}
