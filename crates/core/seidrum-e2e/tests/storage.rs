//! E2E tests for plugin storage (storage.get/set/delete/list).
//!
//! Requires: kernel running with plugin_storage service.
//! Run: cargo test -p seidrum-e2e -- --ignored

mod common;

use seidrum_common::events::{
    StorageDeleteRequest, StorageDeleteResponse, StorageGetRequest, StorageGetResponse,
    StorageListRequest, StorageListResponse, StorageSetRequest, StorageSetResponse,
};

#[tokio::test]
#[ignore] // Requires kernel + kernel running
async fn test_storage_set_and_get() {
    let bus = common::connect_bus().await;
    let key = common::test_id("e2e-storage");

    // Set
    let set_req = StorageSetRequest {
        plugin_id: "e2e-test".into(),
        namespace: "test".into(),
        key: key.clone(),
        value: serde_json::json!({"hello": "world"}),
    };
    let set_resp: StorageSetResponse = common::bus_request(&bus, "storage.set", &set_req).await;
    assert!(set_resp.success);

    // Get
    let get_req = StorageGetRequest {
        plugin_id: "e2e-test".into(),
        namespace: "test".into(),
        key: key.clone(),
    };
    let get_resp: StorageGetResponse = common::bus_request(&bus, "storage.get", &get_req).await;
    assert!(get_resp.found);
    assert_eq!(
        get_resp
            .value
            .unwrap()
            .get("hello")
            .unwrap()
            .as_str()
            .unwrap(),
        "world"
    );

    // Delete
    let del_req = StorageDeleteRequest {
        plugin_id: "e2e-test".into(),
        namespace: "test".into(),
        key: key.clone(),
    };
    let del_resp: StorageDeleteResponse =
        common::bus_request(&bus, "storage.delete", &del_req).await;
    assert!(del_resp.success);
    assert!(del_resp.existed);

    // Verify deleted
    let get_resp2: StorageGetResponse = common::bus_request(&bus, "storage.get", &get_req).await;
    assert!(!get_resp2.found);
}

#[tokio::test]
#[ignore]
async fn test_storage_list_keys() {
    let bus = common::connect_bus().await;
    let key1 = common::test_id("e2e-list");
    let key2 = common::test_id("e2e-list");

    // Set two keys
    for key in [&key1, &key2] {
        let req = StorageSetRequest {
            plugin_id: "e2e-list-test".into(),
            namespace: "test".into(),
            key: key.clone(),
            value: serde_json::json!(1),
        };
        let resp: StorageSetResponse = common::bus_request(&bus, "storage.set", &req).await;
        assert!(resp.success);
    }

    // List
    let list_req = StorageListRequest {
        plugin_id: "e2e-list-test".into(),
        namespace: "test".into(),
    };
    let list_resp: StorageListResponse = common::bus_request(&bus, "storage.list", &list_req).await;
    assert!(list_resp.keys.contains(&key1));
    assert!(list_resp.keys.contains(&key2));

    // Cleanup
    for key in [&key1, &key2] {
        let req = StorageDeleteRequest {
            plugin_id: "e2e-list-test".into(),
            namespace: "test".into(),
            key: key.clone(),
        };
        let _: StorageDeleteResponse = common::bus_request(&bus, "storage.delete", &req).await;
    }
}
