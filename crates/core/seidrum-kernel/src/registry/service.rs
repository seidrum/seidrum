//! Plugin registry service.
//!
//! Subscribes to `plugin.register` and `registry.query` NATS subjects,
//! maintains an in-memory registry of plugin declarations, and provides
//! lookup methods for consumers, producers, and plugin metadata.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{info, warn};

use seidrum_common::events::{EventEnvelope, PluginDeregister, PluginRegister};

/// Thread-safe in-memory plugin registry.
#[derive(Clone)]
pub struct RegistryService {
    /// Map from plugin ID to its registration.
    plugins: Arc<RwLock<HashMap<String, PluginRegister>>>,
}

/// Query types that can be sent to `registry.query`.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "query_type")]
pub enum RegistryQuery {
    /// Get a single plugin by ID.
    #[serde(rename = "get_plugin")]
    GetPlugin { plugin_id: String },
    /// List all registered plugins.
    #[serde(rename = "list_plugins")]
    ListPlugins,
    /// Get plugin IDs that consume a given event type.
    #[serde(rename = "get_consumers")]
    GetConsumers { event_type: String },
    /// Get plugin IDs that produce a given event type.
    #[serde(rename = "get_producers")]
    GetProducers { event_type: String },
    /// Get the config schema for a plugin.
    #[serde(rename = "get_config_schema")]
    GetConfigSchema { plugin_id: String },
}

/// Response to a registry query.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RegistryQueryResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin: Option<PluginRegister>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<Vec<PluginRegister>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RegistryService {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            plugins: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Remove a plugin from the registry.
    pub async fn deregister_plugin(&self, plugin_id: &str) -> bool {
        let mut plugins = self.plugins.write().await;
        let existed = plugins.remove(plugin_id).is_some();
        if existed {
            info!(plugin_id = %plugin_id, "plugin deregistered");
        } else {
            warn!(plugin_id = %plugin_id, "deregister: plugin not found");
        }
        existed
    }

    /// Register a plugin. Overwrites any previous registration with the same ID.
    pub async fn register_plugin(&self, registration: PluginRegister) {
        let id = registration.id.clone();
        let name = registration.name.clone();
        let version = registration.version.clone();
        let consumes_count = registration.consumes.len();
        let produces_count = registration.produces.len();

        let mut plugins = self.plugins.write().await;
        let is_update = plugins.contains_key(&id);
        plugins.insert(id.clone(), registration);

        if is_update {
            info!(
                plugin_id = %id,
                plugin_name = %name,
                version = %version,
                consumes = consumes_count,
                produces = produces_count,
                "plugin re-registered (updated)"
            );
        } else {
            info!(
                plugin_id = %id,
                plugin_name = %name,
                version = %version,
                consumes = consumes_count,
                produces = produces_count,
                "plugin registered"
            );
        }
    }

    /// Get a plugin by ID.
    pub async fn get_plugin(&self, id: &str) -> Option<PluginRegister> {
        let plugins = self.plugins.read().await;
        plugins.get(id).cloned()
    }

    /// List all registered plugins.
    pub async fn list_plugins(&self) -> Vec<PluginRegister> {
        let plugins = self.plugins.read().await;
        plugins.values().cloned().collect()
    }

    /// Get the IDs of plugins that consume a given event type.
    pub async fn get_consumers(&self, event_type: &str) -> Vec<String> {
        let plugins = self.plugins.read().await;
        plugins
            .values()
            .filter(|p| p.consumes.iter().any(|c| c == event_type))
            .map(|p| p.id.clone())
            .collect()
    }

    /// Get the IDs of plugins that produce a given event type.
    pub async fn get_producers(&self, event_type: &str) -> Vec<String> {
        let plugins = self.plugins.read().await;
        plugins
            .values()
            .filter(|p| p.produces.iter().any(|pr| pr == event_type))
            .map(|p| p.id.clone())
            .collect()
    }

    /// Handle a registry query and return the response.
    async fn handle_query(&self, query: RegistryQuery) -> RegistryQueryResponse {
        match query {
            RegistryQuery::GetPlugin { plugin_id } => {
                let plugin = self.get_plugin(&plugin_id).await;
                RegistryQueryResponse {
                    success: plugin.is_some(),
                    plugin,
                    plugins: None,
                    plugin_ids: None,
                    config_schema: None,
                    error: None,
                }
            }
            RegistryQuery::ListPlugins => {
                let plugins = self.list_plugins().await;
                RegistryQueryResponse {
                    success: true,
                    plugin: None,
                    plugins: Some(plugins),
                    plugin_ids: None,
                    config_schema: None,
                    error: None,
                }
            }
            RegistryQuery::GetConsumers { event_type } => {
                let ids = self.get_consumers(&event_type).await;
                RegistryQueryResponse {
                    success: true,
                    plugin: None,
                    plugins: None,
                    plugin_ids: Some(ids),
                    config_schema: None,
                    error: None,
                }
            }
            RegistryQuery::GetProducers { event_type } => {
                let ids = self.get_producers(&event_type).await;
                RegistryQueryResponse {
                    success: true,
                    plugin: None,
                    plugins: None,
                    plugin_ids: Some(ids),
                    config_schema: None,
                    error: None,
                }
            }
            RegistryQuery::GetConfigSchema { plugin_id } => {
                let plugin = self.get_plugin(&plugin_id).await;
                let schema = plugin.as_ref().and_then(|p| p.config_schema.clone());
                RegistryQueryResponse {
                    success: plugin.is_some(),
                    plugin: None,
                    plugins: None,
                    plugin_ids: None,
                    config_schema: schema,
                    error: if plugin.is_none() {
                        Some(format!("Plugin '{}' not found", plugin_id))
                    } else {
                        None
                    },
                }
            }
        }
    }

    /// Spawn the registry service background tasks.
    ///
    /// This subscribes to:
    /// - `plugin.register` — for plugin registration events
    /// - `registry.query` — for request/reply queries about the registry
    ///
    /// Returns a `JoinHandle` for the spawned task.
    pub async fn spawn(
        self,
        nats_client: seidrum_common::bus_client::BusClient,
    ) -> Result<tokio::task::JoinHandle<()>> {
        let mut register_sub = nats_client
            .subscribe("plugin.register".to_string())
            .await
            .context("failed to subscribe to plugin.register")?;
        info!("registry: subscribed to plugin.register");

        let mut deregister_sub = nats_client
            .subscribe("plugin.deregister".to_string())
            .await
            .context("failed to subscribe to plugin.deregister")?;
        info!("registry: subscribed to plugin.deregister");

        let mut query_sub = nats_client
            .subscribe("registry.query".to_string())
            .await
            .context("failed to subscribe to registry.query")?;
        info!("registry: subscribed to registry.query");

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(msg) = register_sub.next() => {
                        // Plugins may publish as raw PluginRegister or wrapped in EventEnvelope
                        let registration = serde_json::from_slice::<PluginRegister>(&msg.payload)
                            .or_else(|_| {
                                let envelope: EventEnvelope = serde_json::from_slice(&msg.payload)?;
                                serde_json::from_value::<PluginRegister>(envelope.payload)
                            });

                        match registration {
                            Ok(reg) => {
                                self.register_plugin(reg).await;
                            }
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    "failed to deserialize plugin.register payload"
                                );
                            }
                        }
                    }
                    Some(msg) = deregister_sub.next() => {
                        match serde_json::from_slice::<PluginDeregister>(&msg.payload) {
                            Ok(dereg) => {
                                self.deregister_plugin(&dereg.id).await;
                            }
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    "failed to deserialize plugin.deregister payload"
                                );
                            }
                        }
                    }
                    Some(msg) = query_sub.next() => {
                        match serde_json::from_slice::<RegistryQuery>(&msg.payload) {
                            Ok(query) => {
                                let response = self.handle_query(query).await;
                                if let Some(reply) = msg.reply {
                                    match serde_json::to_vec(&response) {
                                        Ok(bytes) => {
                                            if let Err(e) = nats_client
                                                .publish_bytes(reply, bytes)
                                                .await
                                            {
                                                warn!(
                                                    error = %e,
                                                    "failed to publish registry.query response"
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            warn!(
                                                error = %e,
                                                "failed to serialize registry.query response"
                                            );
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    "failed to deserialize registry.query payload"
                                );
                                // Send error response if there's a reply subject
                                if let Some(reply) = msg.reply {
                                    let err_resp = RegistryQueryResponse {
                                        success: false,
                                        plugin: None,
                                        plugins: None,
                                        plugin_ids: None,
                                        config_schema: None,
                                        error: Some(format!("invalid query: {e}")),
                                    };
                                    if let Ok(bytes) = serde_json::to_vec(&err_resp) {
                                        let _ = nats_client.publish_bytes(reply, bytes).await;
                                    }
                                }
                            }
                        }
                    }
                    else => break,
                }
            }
            info!("registry service stopped");
        });

        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_and_lookup() {
        let svc = RegistryService::new();

        let reg = PluginRegister {
            id: "telegram".into(),
            name: "Telegram Channel".into(),
            version: "0.1.0".into(),
            description: "Bridges Telegram Bot API to NATS".into(),
            consumes: vec!["channel.telegram.outbound".into()],
            produces: vec!["channel.telegram.inbound".into()],
            health_subject: "plugin.telegram.health".into(),
            consumed_event_types: vec![],
            produced_event_types: vec![],
            config_schema: None,
        };

        svc.register_plugin(reg.clone()).await;

        // get_plugin
        let found = svc.get_plugin("telegram").await;
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.id, "telegram");
        assert_eq!(found.version, "0.1.0");

        // not found
        assert!(svc.get_plugin("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn list_plugins() {
        let svc = RegistryService::new();

        svc.register_plugin(PluginRegister {
            id: "a".into(),
            name: "A".into(),
            version: "0.1.0".into(),
            description: "Plugin A".into(),
            consumes: vec![],
            produces: vec!["event.a".into()],
            health_subject: "plugin.a.health".into(),
            consumed_event_types: vec![],
            produced_event_types: vec![],
            config_schema: None,
        })
        .await;

        svc.register_plugin(PluginRegister {
            id: "b".into(),
            name: "B".into(),
            version: "0.1.0".into(),
            description: "Plugin B".into(),
            consumes: vec!["event.a".into()],
            produces: vec!["event.b".into()],
            health_subject: "plugin.b.health".into(),
            consumed_event_types: vec![],
            produced_event_types: vec![],
            config_schema: None,
        })
        .await;

        let all = svc.list_plugins().await;
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn consumers_and_producers() {
        let svc = RegistryService::new();

        svc.register_plugin(PluginRegister {
            id: "ingester".into(),
            name: "Content Ingester".into(),
            version: "0.1.0".into(),
            description: "Ingests content".into(),
            consumes: vec!["channel.telegram.inbound".into()],
            produces: vec!["brain.content.store".into()],
            health_subject: "plugin.ingester.health".into(),
            consumed_event_types: vec![],
            produced_event_types: vec![],
            config_schema: None,
        })
        .await;

        svc.register_plugin(PluginRegister {
            id: "context-loader".into(),
            name: "Graph Context Loader".into(),
            version: "0.1.0".into(),
            description: "Loads graph context".into(),
            consumes: vec!["channel.telegram.inbound".into()],
            produces: vec!["agent.context.loaded".into()],
            health_subject: "plugin.context-loader.health".into(),
            consumed_event_types: vec![],
            produced_event_types: vec![],
            config_schema: None,
        })
        .await;

        // Two plugins consume channel.telegram.inbound
        let consumers = svc.get_consumers("channel.telegram.inbound").await;
        assert_eq!(consumers.len(), 2);
        assert!(consumers.contains(&"ingester".to_string()));
        assert!(consumers.contains(&"context-loader".to_string()));

        // Only one produces brain.content.store
        let producers = svc.get_producers("brain.content.store").await;
        assert_eq!(producers.len(), 1);
        assert_eq!(producers[0], "ingester");

        // No consumers for unknown event
        assert!(svc.get_consumers("unknown.event").await.is_empty());
    }

    #[tokio::test]
    async fn re_register_overwrites() {
        let svc = RegistryService::new();

        svc.register_plugin(PluginRegister {
            id: "test".into(),
            name: "Test v1".into(),
            version: "0.1.0".into(),
            description: "First version".into(),
            consumes: vec![],
            produces: vec![],
            health_subject: "plugin.test.health".into(),
            consumed_event_types: vec![],
            produced_event_types: vec![],
            config_schema: None,
        })
        .await;

        svc.register_plugin(PluginRegister {
            id: "test".into(),
            name: "Test v2".into(),
            version: "0.2.0".into(),
            description: "Second version".into(),
            consumes: vec!["new.event".into()],
            produces: vec![],
            health_subject: "plugin.test.health".into(),
            consumed_event_types: vec![],
            produced_event_types: vec![],
            config_schema: None,
        })
        .await;

        let p = svc.get_plugin("test").await.unwrap();
        assert_eq!(p.name, "Test v2");
        assert_eq!(p.version, "0.2.0");
        assert_eq!(p.consumes, vec!["new.event".to_string()]);

        // Still only one plugin
        assert_eq!(svc.list_plugins().await.len(), 1);
    }

    #[tokio::test]
    async fn handle_query_get_plugin() {
        let svc = RegistryService::new();

        svc.register_plugin(PluginRegister {
            id: "telegram".into(),
            name: "Telegram".into(),
            version: "0.1.0".into(),
            description: "Telegram plugin".into(),
            consumes: vec![],
            produces: vec![],
            health_subject: "plugin.telegram.health".into(),
            consumed_event_types: vec![],
            produced_event_types: vec![],
            config_schema: None,
        })
        .await;

        let resp = svc
            .handle_query(RegistryQuery::GetPlugin {
                plugin_id: "telegram".into(),
            })
            .await;
        assert!(resp.success);
        assert!(resp.plugin.is_some());

        let resp = svc
            .handle_query(RegistryQuery::GetPlugin {
                plugin_id: "missing".into(),
            })
            .await;
        assert!(!resp.success);
        assert!(resp.plugin.is_none());
    }

    #[test]
    fn query_serialization_roundtrip() {
        let query = RegistryQuery::GetConsumers {
            event_type: "channel.telegram.inbound".into(),
        };
        let json = serde_json::to_string(&query).unwrap();
        let deserialized: RegistryQuery = serde_json::from_str(&json).unwrap();
        match deserialized {
            RegistryQuery::GetConsumers { event_type } => {
                assert_eq!(event_type, "channel.telegram.inbound");
            }
            _ => panic!("wrong variant"),
        }
    }
}
