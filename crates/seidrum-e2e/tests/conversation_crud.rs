//! E2E tests for conversation CRUD (brain.conversation.create/append/get/list/find).

mod common;

use chrono::Utc;
use seidrum_common::events::{
    ConversationAppendRequest, ConversationAppendResponse, ConversationCreateRequest,
    ConversationCreateResponse, ConversationFindRequest, ConversationGetRequest,
    ConversationListRequest, ConversationListResponse, ConversationMessage,
};
use std::collections::HashMap;

#[tokio::test]
#[ignore]
async fn test_conversation_lifecycle() {
    let nats = common::connect_nats().await;
    let test_id = common::test_id("e2e-conv");

    // Create
    let create_req = ConversationCreateRequest {
        platform: "e2e-test".into(),
        participants: vec!["user:tester".into(), "agent:personal-assistant".into()],
        agent_id: "personal-assistant".into(),
        scope: "scope_root".into(),
        metadata: HashMap::from([("test_id".to_string(), test_id.clone())]),
    };
    let create_resp: ConversationCreateResponse =
        common::nats_request(&nats, "brain.conversation.create", &create_req).await;
    let conv_id = create_resp.conversation_id;
    assert!(!conv_id.is_empty());

    // Append user message
    let append_req = ConversationAppendRequest {
        conversation_id: conv_id.clone(),
        message: ConversationMessage {
            role: "user".into(),
            content: Some("Hello, this is an E2E test message".into()),
            tool_calls: vec![],
            tool_results: vec![],
            media: vec![],
            timestamp: Utc::now(),
            active_skills: vec![],
        },
    };
    let append_resp: ConversationAppendResponse =
        common::nats_request(&nats, "brain.conversation.append", &append_req).await;
    assert!(append_resp.success);
    assert_eq!(append_resp.message_count, 1);

    // Append assistant message with active skills
    let append_req2 = ConversationAppendRequest {
        conversation_id: conv_id.clone(),
        message: ConversationMessage {
            role: "assistant".into(),
            content: Some("Hello! I received your test message.".into()),
            tool_calls: vec![],
            tool_results: vec![],
            media: vec![],
            timestamp: Utc::now(),
            active_skills: vec!["code-review".into()],
        },
    };
    let append_resp2: ConversationAppendResponse =
        common::nats_request(&nats, "brain.conversation.append", &append_req2).await;
    assert!(append_resp2.success);
    assert_eq!(append_resp2.message_count, 2);

    // Get
    let get_req = ConversationGetRequest {
        conversation_id: conv_id.clone(),
        max_messages: 0,
    };
    let get_resp: serde_json::Value =
        common::nats_request(&nats, "brain.conversation.get", &get_req).await;
    assert!(!get_resp.is_null());
    let messages = get_resp.get("messages").unwrap().as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].get("role").unwrap().as_str().unwrap(), "user");
    assert_eq!(
        messages[1].get("role").unwrap().as_str().unwrap(),
        "assistant"
    );

    // List
    let list_req = ConversationListRequest {
        agent_id: "personal-assistant".into(),
        platform: Some("e2e-test".into()),
        limit: 10,
    };
    let list_resp: ConversationListResponse =
        common::nats_request(&nats, "brain.conversation.list", &list_req).await;
    assert!(list_resp.conversations.iter().any(|c| c.id == conv_id));

    // Find by metadata
    let find_req = ConversationFindRequest {
        agent_id: "personal-assistant".into(),
        platform: "e2e-test".into(),
        metadata_key: "test_id".into(),
        metadata_value: test_id.clone(),
    };
    let find_resp: serde_json::Value =
        common::nats_request(&nats, "brain.conversation.find", &find_req).await;
    assert!(!find_resp.is_null());
    assert_eq!(find_resp.get("id").unwrap().as_str().unwrap(), conv_id);
}

#[tokio::test]
#[ignore]
async fn test_conversation_get_with_max_messages() {
    let nats = common::connect_nats().await;

    // Create
    let create_req = ConversationCreateRequest {
        platform: "e2e-test".into(),
        participants: vec!["user:tester".into()],
        agent_id: "test-agent".into(),
        scope: "scope_root".into(),
        metadata: HashMap::new(),
    };
    let create_resp: ConversationCreateResponse =
        common::nats_request(&nats, "brain.conversation.create", &create_req).await;
    let conv_id = create_resp.conversation_id;

    // Append 5 messages
    for i in 0..5 {
        let req = ConversationAppendRequest {
            conversation_id: conv_id.clone(),
            message: ConversationMessage {
                role: "user".into(),
                content: Some(format!("Message {}", i)),
                tool_calls: vec![],
                tool_results: vec![],
                media: vec![],
                timestamp: Utc::now(),
                active_skills: vec![],
            },
        };
        let _: ConversationAppendResponse =
            common::nats_request(&nats, "brain.conversation.append", &req).await;
    }

    // Get with max_messages=2
    let get_req = ConversationGetRequest {
        conversation_id: conv_id.clone(),
        max_messages: 2,
    };
    let get_resp: serde_json::Value =
        common::nats_request(&nats, "brain.conversation.get", &get_req).await;
    let messages = get_resp.get("messages").unwrap().as_array().unwrap();
    assert_eq!(messages.len(), 2);
    // Should be the last 2 messages
    assert!(messages[0]
        .get("content")
        .unwrap()
        .as_str()
        .unwrap()
        .contains("Message 3"));
    assert!(messages[1]
        .get("content")
        .unwrap()
        .as_str()
        .unwrap()
        .contains("Message 4"));
}
