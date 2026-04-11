//! Connection manager: tracks external plugin WebSocket connections and pending requests.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::protocol::ServerMessage;

/// An active external plugin WebSocket connection.
#[allow(dead_code)]
pub struct ExternalPluginConnection {
    pub plugin_id: String,
    pub registered_at: Instant,
    pub sender: mpsc::UnboundedSender<ServerMessage>,
    /// NATS subscription tasks spawned for this connection.
    pub subscriptions: Mutex<Vec<JoinHandle<()>>>,
    pub events_processed: AtomicU64,
}

/// A pending bus request/reply awaiting a response from an external plugin.
pub struct PendingRequest {
    pub reply_subject: seidrum_common::bus_client::Subject,
    pub deadline: Instant,
    pub nats_client: seidrum_common::bus_client::BusClient,
    /// Whether this is a health check (shorter timeout, different error response).
    pub is_health_check: bool,
}

/// Thread-safe manager for external plugin connections and pending requests.
#[derive(Clone)]
pub struct ConnectionManager {
    inner: Arc<ConnectionManagerInner>,
}

struct ConnectionManagerInner {
    connections: DashMap<String, ExternalPluginConnection>,
    pending: DashMap<String, PendingRequest>,
}

impl ConnectionManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ConnectionManagerInner {
                connections: DashMap::new(),
                pending: DashMap::new(),
            }),
        }
    }

    /// Register a new external plugin connection. Returns error if already registered.
    pub fn register(
        &self,
        plugin_id: String,
        sender: mpsc::UnboundedSender<ServerMessage>,
    ) -> Result<(), String> {
        if self.inner.connections.contains_key(&plugin_id) {
            return Err(format!("Plugin '{}' is already connected", plugin_id));
        }

        self.inner.connections.insert(
            plugin_id.clone(),
            ExternalPluginConnection {
                plugin_id,
                registered_at: Instant::now(),
                sender,
                subscriptions: Mutex::new(Vec::new()),
                events_processed: AtomicU64::new(0),
            },
        );
        Ok(())
    }

    /// Deregister a plugin connection, aborting its subscription tasks.
    pub async fn deregister(&self, plugin_id: &str) -> bool {
        if let Some((_, conn)) = self.inner.connections.remove(plugin_id) {
            let mut subs = conn.subscriptions.lock().await;
            for handle in subs.drain(..) {
                handle.abort();
            }
            info!(%plugin_id, "External plugin connection removed");
            true
        } else {
            false
        }
    }

    /// Get the sender channel for a connected plugin.
    pub fn get_sender(&self, plugin_id: &str) -> Option<mpsc::UnboundedSender<ServerMessage>> {
        self.inner
            .connections
            .get(plugin_id)
            .map(|c| c.sender.clone())
    }

    /// Add a subscription task handle to an existing connection.
    pub async fn add_subscription(&self, plugin_id: &str, handle: JoinHandle<()>) {
        if let Some(conn) = self.inner.connections.get(plugin_id) {
            conn.subscriptions.lock().await.push(handle);
        }
    }

    /// Increment the events processed counter for a plugin.
    pub fn increment_events(&self, plugin_id: &str) {
        if let Some(conn) = self.inner.connections.get(plugin_id) {
            conn.events_processed.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Store a pending request awaiting a response from an external plugin.
    pub fn add_pending(&self, request_id: String, pending: PendingRequest) {
        self.inner.pending.insert(request_id, pending);
    }

    /// Take a pending request by ID (removing it from the map).
    pub fn take_pending(&self, request_id: &str) -> Option<PendingRequest> {
        self.inner.pending.remove(request_id).map(|(_, v)| v)
    }

    /// Reap expired pending requests, sending error responses back on NATS.
    pub async fn reap_expired(&self) {
        let now = Instant::now();
        let mut expired_ids = Vec::new();

        for entry in self.inner.pending.iter() {
            if now >= entry.value().deadline {
                expired_ids.push(entry.key().clone());
            }
        }

        for request_id in expired_ids {
            if let Some((_, pending)) = self.inner.pending.remove(&request_id) {
                let response = if pending.is_health_check {
                    serde_json::json!({
                        "plugin_id": "unknown",
                        "status": "unhealthy",
                        "uptime_seconds": 0,
                        "events_processed": 0,
                        "last_error": "External plugin did not respond to health check in time"
                    })
                } else {
                    serde_json::json!({
                        "tool_id": "unknown",
                        "result": {"error": "External plugin timed out"},
                        "is_error": true
                    })
                };

                if let Ok(bytes) = serde_json::to_vec(&response) {
                    if let Err(e) = pending
                        .nats_client
                        .reply_to(&pending.reply_subject, bytes)
                        .await
                    {
                        warn!(error = %e, %request_id, "Failed to publish timeout response");
                    }
                }
            }
        }
    }

    /// List all connected plugin IDs.
    pub fn list_plugin_ids(&self) -> Vec<String> {
        self.inner
            .connections
            .iter()
            .map(|e| e.key().clone())
            .collect()
    }

    /// Number of connected external plugins.
    pub fn connected_count(&self) -> usize {
        self.inner.connections.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_and_deregister() {
        let mgr = ConnectionManager::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        mgr.register("test".into(), tx).unwrap();
        assert_eq!(mgr.connected_count(), 1);
        assert!(mgr.deregister("test").await);
        assert_eq!(mgr.connected_count(), 0);
    }

    #[test]
    fn duplicate_register_fails() {
        let mgr = ConnectionManager::new();
        let (tx1, _) = mpsc::unbounded_channel();
        let (tx2, _) = mpsc::unbounded_channel();
        mgr.register("test".into(), tx1).unwrap();
        assert!(mgr.register("test".into(), tx2).is_err());
    }

    #[test]
    fn get_sender_unknown() {
        let mgr = ConnectionManager::new();
        assert!(mgr.get_sender("nonexistent").is_none());
    }

    #[test]
    fn pending_take_nonexistent() {
        let mgr = ConnectionManager::new();
        assert!(mgr.take_pending("nonexistent").is_none());
    }

    #[test]
    fn list_plugins() {
        let mgr = ConnectionManager::new();
        let (tx1, _) = mpsc::unbounded_channel();
        let (tx2, _) = mpsc::unbounded_channel();
        mgr.register("alpha".into(), tx1).unwrap();
        mgr.register("beta".into(), tx2).unwrap();

        let ids = mgr.list_plugin_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"alpha".to_string()));
        assert!(ids.contains(&"beta".to_string()));
    }
}
