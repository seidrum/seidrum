//! Plugin storage service: persistent key-value store per plugin.
//!
//! Subscribes to `storage.get`, `storage.set`, `storage.delete`, and
//! `storage.list` NATS subjects (all request/reply), backed by the
//! `plugin_storage` ArangoDB collection.

use anyhow::{Context, Result};
use seidrum_common::events::{
    StorageDeleteRequest, StorageDeleteResponse, StorageGetRequest, StorageGetResponse,
    StorageListRequest, StorageListResponse, StorageSetRequest, StorageSetResponse,
};
use tracing::{info, warn};

use crate::brain::client::ArangoClient;

/// Persistent key-value storage for plugins.
pub struct PluginStorageService {
    arango: ArangoClient,
}

impl PluginStorageService {
    pub fn new(arango: ArangoClient) -> Self {
        Self { arango }
    }

    /// Build the compound document key: `{plugin_id}:{namespace}:{key}`.
    fn doc_key(plugin_id: &str, namespace: &str, key: &str) -> String {
        format!("{}:{}:{}", plugin_id, namespace, key)
    }

    async fn handle_get(&self, req: StorageGetRequest) -> StorageGetResponse {
        let doc_key = Self::doc_key(&req.plugin_id, &req.namespace, &req.key);
        match self.arango.get_document("plugin_storage", &doc_key).await {
            Ok(Some(doc)) => {
                let value = doc.get("value").cloned();
                StorageGetResponse {
                    found: value.is_some(),
                    value,
                }
            }
            Ok(None) => StorageGetResponse {
                found: false,
                value: None,
            },
            Err(err) => {
                warn!(%err, %doc_key, "storage.get failed");
                StorageGetResponse {
                    found: false,
                    value: None,
                }
            }
        }
    }

    async fn handle_set(&self, req: StorageSetRequest) -> StorageSetResponse {
        let doc_key = Self::doc_key(&req.plugin_id, &req.namespace, &req.key);
        let doc = serde_json::json!({
            "plugin_id": req.plugin_id,
            "namespace": req.namespace,
            "key": req.key,
            "value": req.value,
            "updated_at": chrono::Utc::now().to_rfc3339(),
        });

        match self
            .arango
            .upsert_document("plugin_storage", &doc_key, &doc)
            .await
        {
            Ok(_) => StorageSetResponse {
                success: true,
                error: None,
            },
            Err(err) => {
                warn!(%err, %doc_key, "storage.set failed");
                StorageSetResponse {
                    success: false,
                    error: Some(err.to_string()),
                }
            }
        }
    }

    async fn handle_delete(&self, req: StorageDeleteRequest) -> StorageDeleteResponse {
        let doc_key = Self::doc_key(&req.plugin_id, &req.namespace, &req.key);

        // Check if it exists first, then remove via AQL
        let query = r#"
            LET existing = DOCUMENT(CONCAT("plugin_storage/", @key))
            FILTER existing != null
            REMOVE existing IN plugin_storage
            RETURN true
        "#;
        let bind_vars = serde_json::json!({ "key": doc_key });

        match self.arango.execute_aql(query, &bind_vars).await {
            Ok(resp) => {
                let removed = resp
                    .get("result")
                    .and_then(|v| v.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false);
                StorageDeleteResponse {
                    success: true,
                    existed: removed,
                }
            }
            Err(err) => {
                warn!(%err, %doc_key, "storage.delete failed");
                StorageDeleteResponse {
                    success: false,
                    existed: false,
                }
            }
        }
    }

    async fn handle_list(&self, req: StorageListRequest) -> StorageListResponse {
        let prefix = format!("{}:{}", req.plugin_id, req.namespace);
        let query = r#"
            FOR doc IN plugin_storage
                FILTER STARTS_WITH(doc._key, @prefix)
                RETURN doc.key
        "#;
        let bind_vars = serde_json::json!({ "prefix": prefix });

        match self.arango.execute_aql(query, &bind_vars).await {
            Ok(resp) => {
                let keys = resp
                    .get("result")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                StorageListResponse { keys }
            }
            Err(err) => {
                warn!(%err, plugin_id = %req.plugin_id, namespace = %req.namespace, "storage.list failed");
                StorageListResponse { keys: vec![] }
            }
        }
    }

    /// Spawn the service: subscribe to storage NATS subjects and handle requests.
    pub async fn spawn(
        self,
        nats_client: seidrum_common::bus_client::BusClient,
    ) -> Result<tokio::task::JoinHandle<()>> {
        let mut get_sub = nats_client
            .subscribe("storage.get".to_string())
            .await
            .context("failed to subscribe to storage.get")?;

        let mut set_sub = nats_client
            .subscribe("storage.set".to_string())
            .await
            .context("failed to subscribe to storage.set")?;

        let mut delete_sub = nats_client
            .subscribe("storage.delete".to_string())
            .await
            .context("failed to subscribe to storage.delete")?;

        let mut list_sub = nats_client
            .subscribe("storage.list".to_string())
            .await
            .context("failed to subscribe to storage.list")?;

        info!("plugin_storage: subscribed to storage.{{get,set,delete,list}}");

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(msg) = get_sub.next() => {
                        match serde_json::from_slice::<StorageGetRequest>(&msg.payload) {
                            Ok(req) => {
                                let resp = self.handle_get(req).await;
                                if let Some(reply) = msg.reply {
                                    if let Ok(bytes) = serde_json::to_vec(&resp) {
                                        let _ = nats_client.publish_bytes(reply, bytes).await;
                                    }
                                }
                            }
                            Err(e) => warn!(error = %e, "failed to deserialize storage.get"),
                        }
                    }
                    Some(msg) = set_sub.next() => {
                        match serde_json::from_slice::<StorageSetRequest>(&msg.payload) {
                            Ok(req) => {
                                let resp = self.handle_set(req).await;
                                if let Some(reply) = msg.reply {
                                    if let Ok(bytes) = serde_json::to_vec(&resp) {
                                        let _ = nats_client.publish_bytes(reply, bytes).await;
                                    }
                                }
                            }
                            Err(e) => warn!(error = %e, "failed to deserialize storage.set"),
                        }
                    }
                    Some(msg) = delete_sub.next() => {
                        match serde_json::from_slice::<StorageDeleteRequest>(&msg.payload) {
                            Ok(req) => {
                                let resp = self.handle_delete(req).await;
                                if let Some(reply) = msg.reply {
                                    if let Ok(bytes) = serde_json::to_vec(&resp) {
                                        let _ = nats_client.publish_bytes(reply, bytes).await;
                                    }
                                }
                            }
                            Err(e) => warn!(error = %e, "failed to deserialize storage.delete"),
                        }
                    }
                    Some(msg) = list_sub.next() => {
                        match serde_json::from_slice::<StorageListRequest>(&msg.payload) {
                            Ok(req) => {
                                let resp = self.handle_list(req).await;
                                if let Some(reply) = msg.reply {
                                    if let Ok(bytes) = serde_json::to_vec(&resp) {
                                        let _ = nats_client.publish_bytes(reply, bytes).await;
                                    }
                                }
                            }
                            Err(e) => warn!(error = %e, "failed to deserialize storage.list"),
                        }
                    }
                    else => break,
                }
            }
            info!("plugin_storage service stopped");
        });

        Ok(handle)
    }
}
