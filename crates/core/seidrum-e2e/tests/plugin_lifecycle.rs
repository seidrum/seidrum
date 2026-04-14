//! E2E tests for plugin registration and deregistration.
//!
//! Requires: kernel running with registry service.

mod common;

use seidrum_common::events::{PluginDeregister, PluginRegister};
use serde::Deserialize;
use std::time::Duration;

/// Registry query types (defined in kernel, not exported from common).
#[derive(serde::Serialize)]
#[serde(tag = "query_type")]
enum RegistryQuery {
    #[serde(rename = "list_plugins")]
    ListPlugins,
    #[serde(rename = "get_plugin")]
    GetPlugin { plugin_id: String },
}

#[derive(Deserialize)]
struct RegistryQueryResponse {
    success: bool,
    plugin: Option<serde_json::Value>,
    plugins: Option<Vec<serde_json::Value>>,
}

#[tokio::test]
#[ignore]
async fn test_plugin_register_and_query() {
    let bus = common::connect_bus().await;
    let plugin_id = common::test_id("e2e-plugin");

    // Register (raw payload — kernel accepts both raw and envelope-wrapped)
    let register = PluginRegister {
        id: plugin_id.clone(),
        name: "E2E Test Plugin".into(),
        version: "0.1.0".into(),
        description: "Plugin created by E2E tests".into(),
        consumes: vec![],
        produces: vec![],
        health_subject: format!("plugin.{}.health", plugin_id),
        consumed_event_types: vec![],
        produced_event_types: vec![],
        config_schema: None,
    };

    let bytes = serde_json::to_vec(&register).unwrap();
    bus.publish_bytes("plugin.register", bytes).await.unwrap();

    // Give registry time to process
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Query - get specific plugin
    let query = RegistryQuery::GetPlugin {
        plugin_id: plugin_id.clone(),
    };
    let resp: RegistryQueryResponse = common::bus_request(&bus, "registry.query", &query).await;
    assert!(resp.success, "Plugin should be found in registry");
    assert!(resp.plugin.is_some());
    let plugin = resp.plugin.unwrap();
    assert_eq!(plugin.get("id").unwrap().as_str().unwrap(), plugin_id);
    assert_eq!(
        plugin.get("name").unwrap().as_str().unwrap(),
        "E2E Test Plugin"
    );

    // Query - list all plugins (should include ours)
    let list_query = RegistryQuery::ListPlugins;
    let list_resp: RegistryQueryResponse =
        common::bus_request(&bus, "registry.query", &list_query).await;
    assert!(list_resp.success);
    let plugins = list_resp.plugins.unwrap();
    assert!(
        plugins
            .iter()
            .any(|p| p.get("id").unwrap().as_str().unwrap() == plugin_id),
        "Plugin should appear in list"
    );

    // Deregister
    let dereg = PluginDeregister {
        id: plugin_id.clone(),
    };
    let dereg_bytes = serde_json::to_vec(&dereg).unwrap();
    bus.publish_bytes("plugin.deregister", dereg_bytes)
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify deregistered
    let query2 = RegistryQuery::GetPlugin {
        plugin_id: plugin_id.clone(),
    };
    let resp2: RegistryQueryResponse = common::bus_request(&bus, "registry.query", &query2).await;
    assert!(!resp2.success, "Plugin should no longer be in registry");
    assert!(resp2.plugin.is_none());
}
