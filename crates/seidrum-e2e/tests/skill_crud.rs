//! E2E tests for skill CRUD (brain.skill.save/search/get/list/delete).
//!
//! Requires: NATS + kernel running with brain service.

mod common;

use seidrum_common::events::{
    SkillGetRequest, SkillListRequest, SkillListResponse, SkillSaveRequest, SkillSaveResponse,
    SkillSearchRequest, SkillSearchResponse,
};

#[tokio::test]
#[ignore]
async fn test_skill_save_and_get() {
    let nats = common::connect_nats().await;
    let skill_id = common::test_id("e2e-skill");

    // Save
    let save_req = SkillSaveRequest {
        id: Some(skill_id.clone()),
        description: "E2E test skill for code review".into(),
        snippet: "When reviewing code, always check for security issues first.".into(),
        source: "system".into(),
        scope: None,
        tags: vec!["e2e".into(), "test".into()],
        learned_from: None,
        embedding: vec![],
    };
    let save_resp: SkillSaveResponse =
        common::nats_request(&nats, "brain.skill.save", &save_req).await;
    assert_eq!(save_resp.skill_id, skill_id);
    assert!(save_resp.is_new);

    // Get
    let get_req = SkillGetRequest {
        skill_id: skill_id.clone(),
    };
    let get_resp: serde_json::Value =
        common::nats_request(&nats, "brain.skill.get", &get_req).await;
    assert!(!get_resp.is_null());
    assert_eq!(get_resp.get("id").unwrap().as_str().unwrap(), skill_id);
    assert!(get_resp
        .get("snippet")
        .unwrap()
        .as_str()
        .unwrap()
        .contains("security issues"));

    // Cleanup
    let _: serde_json::Value = common::nats_request(&nats, "brain.skill.delete", &get_req).await;
}

#[tokio::test]
#[ignore]
async fn test_skill_search() {
    let nats = common::connect_nats().await;
    let skill_id = common::test_id("e2e-search");

    // Save a skill
    let save_req = SkillSaveRequest {
        id: Some(skill_id.clone()),
        description: "Database migration best practices for E2E testing".into(),
        snippet: "Always backup before migrating.".into(),
        source: "system".into(),
        scope: None,
        tags: vec!["database".into()],
        learned_from: None,
        embedding: vec![],
    };
    let _: SkillSaveResponse = common::nats_request(&nats, "brain.skill.save", &save_req).await;

    // Search
    let search_req = SkillSearchRequest {
        query: "database migration".into(),
        limit: Some(10),
        scope: None,
    };
    let search_resp: SkillSearchResponse =
        common::nats_request(&nats, "brain.skill.search", &search_req).await;
    assert!(
        search_resp.skills.iter().any(|s| s.id == skill_id),
        "Expected to find skill {} in search results",
        skill_id
    );

    // Cleanup
    let get_req = SkillGetRequest { skill_id };
    let _: serde_json::Value = common::nats_request(&nats, "brain.skill.delete", &get_req).await;
}

#[tokio::test]
#[ignore]
async fn test_skill_list() {
    let nats = common::connect_nats().await;

    let list_req = SkillListRequest {
        source_filter: None,
        limit: Some(100),
    };
    let list_resp: SkillListResponse =
        common::nats_request(&nats, "brain.skill.list", &list_req).await;

    // Verify the response deserializes correctly — the list may be empty on a clean test DB
    let _ = list_resp.skills.len();
}
