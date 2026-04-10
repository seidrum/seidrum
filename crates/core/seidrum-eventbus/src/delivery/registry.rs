//! Custom delivery channel registry.
//!
//! Allows registration, lookup, and unregistration of delivery channels
//! by type name, enabling pluggable delivery backends.

use super::DeliveryChannel;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Registry for custom delivery channels keyed by channel type string.
///
/// Thread-safe: all methods acquire a [`tokio::sync::RwLock`] internally, so
/// the registry can be shared across tasks via `Arc<ChannelRegistry>` or by
/// cloning (clones share the same underlying map).
pub struct ChannelRegistry {
    channels: Arc<RwLock<HashMap<String, Arc<dyn DeliveryChannel>>>>,
}

impl ChannelRegistry {
    /// Create a new, empty channel registry.
    pub fn new() -> Self {
        Self {
            channels: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a delivery channel by type name.
    ///
    /// If a channel was already registered under the same type name it is
    /// silently replaced; a warning is logged so accidental shadowing is
    /// visible in operator logs.
    pub async fn register(&self, type_name: &str, channel: Arc<dyn DeliveryChannel>) {
        let mut channels = self.channels.write().await;
        if channels.contains_key(type_name) {
            tracing::warn!(
                channel_type = type_name,
                "ChannelRegistry: replacing existing registration"
            );
        }
        channels.insert(type_name.to_string(), channel);
    }

    /// Look up a registered delivery channel by type name.
    pub async fn get(&self, type_name: &str) -> Option<Arc<dyn DeliveryChannel>> {
        let channels = self.channels.read().await;
        channels.get(type_name).cloned()
    }

    /// Unregister a delivery channel by type name.
    pub async fn unregister(&self, type_name: &str) {
        let mut channels = self.channels.write().await;
        channels.remove(type_name);
    }

    /// List all registered channel types.
    pub async fn list_types(&self) -> Vec<String> {
        let channels = self.channels.read().await;
        channels.keys().cloned().collect()
    }
}

impl Clone for ChannelRegistry {
    fn clone(&self) -> Self {
        Self {
            channels: Arc::clone(&self.channels),
        }
    }
}

impl Default for ChannelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::{DeliveryError, DeliveryReceipt, DeliveryResult};
    use async_trait::async_trait;

    struct DummyChannel;

    #[async_trait]
    impl DeliveryChannel for DummyChannel {
        async fn deliver(
            &self,
            _event: &[u8],
            _subject: &str,
            _config: &crate::delivery::ChannelConfig,
        ) -> DeliveryResult<DeliveryReceipt> {
            Err(DeliveryError::NotReady)
        }

        async fn cleanup(&self, _config: &crate::delivery::ChannelConfig) -> DeliveryResult<()> {
            Ok(())
        }

        async fn is_healthy(&self, _config: &crate::delivery::ChannelConfig) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn test_register_and_get() {
        let registry = ChannelRegistry::new();
        let channel: Arc<dyn DeliveryChannel> = Arc::new(DummyChannel);

        registry.register("test", Arc::clone(&channel)).await;
        assert!(registry.get("test").await.is_some());
        assert!(registry.get("notfound").await.is_none());
    }

    #[tokio::test]
    async fn test_unregister() {
        let registry = ChannelRegistry::new();
        let channel: Arc<dyn DeliveryChannel> = Arc::new(DummyChannel);

        registry.register("test", channel).await;
        assert!(registry.get("test").await.is_some());

        registry.unregister("test").await;
        assert!(registry.get("test").await.is_none());
    }

    #[tokio::test]
    async fn test_list_types() {
        let registry = ChannelRegistry::new();
        let channel: Arc<dyn DeliveryChannel> = Arc::new(DummyChannel);

        registry.register("test1", Arc::clone(&channel)).await;
        registry.register("test2", Arc::clone(&channel)).await;

        let types = registry.list_types().await;
        assert_eq!(types.len(), 2);
        assert!(types.contains(&"test1".to_string()));
        assert!(types.contains(&"test2".to_string()));
    }
}
