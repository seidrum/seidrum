//! Audit logging for API gateway events.

use chrono::{DateTime, Utc};
use seidrum_common::bus_client::BusClient;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Audit log entry recording mutations and auth events.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AuditEntry {
    pub timestamp: DateTime<Utc>,
    pub action: String,   // "auth.login", "config.update", "plugin.register", etc.
    pub subject: String,  // who did it (user ID or API key)
    pub resource: String, // what was affected
    pub method: String,   // HTTP method
    pub path: String,     // request path
    pub status: u16,      // response status code
    pub details: Option<String>, // additional context
    /// User ID for multi-tenant audit trails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

/// Audit log with in-memory buffer and ArangoDB persistence via NATS.
#[derive(Clone)]
pub struct AuditLog {
    entries: Arc<RwLock<VecDeque<AuditEntry>>>,
    max_entries: usize,
    nats: Option<BusClient>,
}

impl AuditLog {
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: Arc::new(RwLock::new(VecDeque::with_capacity(max_entries))),
            max_entries,
            nats: None,
        }
    }

    /// Create an audit log that also persists entries to ArangoDB via NATS.
    pub fn with_nats(max_entries: usize, nats: BusClient) -> Self {
        Self {
            entries: Arc::new(RwLock::new(VecDeque::with_capacity(max_entries))),
            max_entries,
            nats: Some(nats),
        }
    }

    /// Log an audit entry. If the buffer is full, drops the oldest entry.
    /// Also persists to ArangoDB via NATS if a NATS client is configured.
    pub async fn log(&self, entry: AuditEntry) {
        info!(
            action = %entry.action,
            subject = %entry.subject,
            resource = %entry.resource,
            method = %entry.method,
            path = %entry.path,
            status = entry.status,
            "Audit log entry"
        );

        // Persist to ArangoDB via NATS (fire-and-forget)
        if let Some(nats) = &self.nats {
            let store_req = serde_json::json!({
                "entry": entry,
            });
            if let Err(e) = nats
                .publish_envelope("brain.audit.store", None, None, &store_req)
                .await
            {
                warn!(error = %e, "Failed to persist audit entry to brain");
            }
        }

        let mut entries = self.entries.write().await;
        entries.push_back(entry);

        // Keep only the most recent max_entries
        while entries.len() > self.max_entries {
            entries.pop_front();
        }
    }

    /// Query recent audit entries, optionally filtered by timestamp.
    pub async fn query(&self, limit: usize, since: Option<DateTime<Utc>>) -> Vec<AuditEntry> {
        let entries = self.entries.read().await;
        let mut result: Vec<AuditEntry> = entries
            .iter()
            .filter(|e| since.map(|s| e.timestamp >= s).unwrap_or(true))
            .cloned()
            .collect();

        // Return most recent entries first
        result.reverse();

        if limit > 0 {
            result.truncate(limit);
        }

        result
    }

    /// Get the total number of entries in the log.
    pub async fn count(&self) -> usize {
        self.entries.read().await.len()
    }

    /// Clear all entries (useful for testing).
    #[allow(dead_code)]
    pub async fn clear(&self) {
        self.entries.write().await.clear();
    }
}

/// Helper to construct an audit entry.
pub struct AuditEntryBuilder {
    pub action: String,
    pub subject: String,
    pub resource: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub details: Option<String>,
    pub user_id: Option<String>,
}

impl AuditEntryBuilder {
    pub fn new(action: &str, subject: &str, resource: &str) -> Self {
        Self {
            action: action.to_string(),
            subject: subject.to_string(),
            resource: resource.to_string(),
            method: String::new(),
            path: String::new(),
            status: 200,
            details: None,
            user_id: None,
        }
    }

    pub fn method(mut self, method: &str) -> Self {
        self.method = method.to_string();
        self
    }

    pub fn path(mut self, path: &str) -> Self {
        self.path = path.to_string();
        self
    }

    pub fn status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    pub fn details(mut self, details: &str) -> Self {
        self.details = Some(details.to_string());
        self
    }

    pub fn user_id(mut self, user_id: Option<String>) -> Self {
        self.user_id = user_id;
        self
    }

    pub fn build(self) -> AuditEntry {
        AuditEntry {
            timestamp: Utc::now(),
            action: self.action,
            subject: self.subject,
            resource: self.resource,
            method: self.method,
            path: self.path,
            status: self.status,
            details: self.details,
            user_id: self.user_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_audit_log_basic() {
        let log = AuditLog::new(10);
        let entry = AuditEntryBuilder::new("test.action", "user1", "resource1")
            .method("POST")
            .path("/api/test")
            .status(200)
            .build();

        log.log(entry.clone()).await;
        assert_eq!(log.count().await, 1);

        let results = log.query(10, None).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].action, "test.action");
    }

    #[tokio::test]
    async fn test_audit_log_max_entries() {
        let log = AuditLog::new(3);

        for i in 0..5 {
            let entry = AuditEntryBuilder::new("action", &format!("user{}", i), "resource")
                .method("POST")
                .build();
            log.log(entry).await;
        }

        assert_eq!(log.count().await, 3);
    }

    #[tokio::test]
    async fn test_audit_log_query_ordering() {
        let log = AuditLog::new(10);

        for i in 0..3 {
            let entry = AuditEntryBuilder::new("action", &format!("user{}", i), "resource").build();
            log.log(entry).await;
        }

        let results = log.query(10, None).await;
        // Should be in reverse order (most recent first)
        assert_eq!(results[0].subject, "user2");
        assert_eq!(results[1].subject, "user1");
        assert_eq!(results[2].subject, "user0");
    }
}
