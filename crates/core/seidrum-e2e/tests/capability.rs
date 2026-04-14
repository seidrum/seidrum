//! E2E tests for capability registration and search.
//!
//! Requires: kernel running with tool registry service.

mod common;

use seidrum_common::events::{
    ToolDescribeRequest, ToolDescribeResponse, ToolRegister, ToolSearchRequest, ToolSearchResponse,
};
use std::time::Duration;

#[tokio::test]
#[ignore]
async fn test_capability_register_and_search() {
    let bus = common::connect_bus().await;
    let tool_id = common::test_id("e2e-tool");
    let plugin_id = common::test_id("e2e-plugin");

    // Register capability
    let register = ToolRegister {
        tool_id: tool_id.clone(),
        plugin_id: plugin_id.clone(),
        name: "E2E Test Tool".into(),
        summary_md: "A tool created by E2E tests for testing capability registration".into(),
        manual_md: "# E2E Test Tool\n\nUsed for testing.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "input": { "type": "string" }
            }
        }),
        call_subject: format!("capability.call.{}", plugin_id),
        kind: "tool".into(),
    };

    let bytes = serde_json::to_vec(&register).unwrap();
    bus.publish_bytes("capability.register", bytes)
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Search for it
    let search_req = ToolSearchRequest {
        query_text: "E2E test".into(),
        limit: Some(20),
        kind_filter: None,
    };
    let search_resp: ToolSearchResponse =
        common::bus_request(&bus, "capability.search", &search_req).await;
    assert!(
        search_resp.tools.iter().any(|t| t.tool_id == tool_id),
        "Tool {} should appear in search results",
        tool_id
    );

    // Describe it
    let describe_req = ToolDescribeRequest {
        tool_id: tool_id.clone(),
    };
    let describe_resp: ToolDescribeResponse =
        common::bus_request(&bus, "capability.describe", &describe_req).await;
    assert_eq!(describe_resp.tool_id, tool_id);
    assert_eq!(describe_resp.plugin_id, plugin_id);
    assert_eq!(describe_resp.name, "E2E Test Tool");
}

#[tokio::test]
#[ignore]
async fn test_capability_kind_filter() {
    let bus = common::connect_bus().await;
    let tool_id = common::test_id("e2e-cmd");

    // Register a command-type capability
    let register = ToolRegister {
        tool_id: tool_id.clone(),
        plugin_id: "e2e-test".into(),
        name: "E2E Command".into(),
        summary_md: "A command for E2E kind filter testing".into(),
        manual_md: "".into(),
        parameters: serde_json::json!({}),
        call_subject: "capability.call.e2e-test".into(),
        kind: "command".into(),
    };

    let bytes = serde_json::to_vec(&register).unwrap();
    bus.publish_bytes("capability.register", bytes)
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Search with kind_filter = "command"
    let search_req = ToolSearchRequest {
        query_text: "".into(),
        limit: Some(50),
        kind_filter: Some("command".into()),
    };
    let search_resp: ToolSearchResponse =
        common::bus_request(&bus, "capability.search", &search_req).await;
    assert!(
        search_resp.tools.iter().any(|t| t.tool_id == tool_id),
        "Command tool should appear with kind_filter=command"
    );

    // Search with kind_filter = "tool" should NOT include our command
    let search_req2 = ToolSearchRequest {
        query_text: "".into(),
        limit: Some(50),
        kind_filter: Some("tool".into()),
    };
    let search_resp2: ToolSearchResponse =
        common::bus_request(&bus, "capability.search", &search_req2).await;
    assert!(
        !search_resp2.tools.iter().any(|t| t.tool_id == tool_id),
        "Command tool should NOT appear with kind_filter=tool"
    );
}
