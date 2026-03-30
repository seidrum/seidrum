//! Audit logging for API gateway events.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

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
}

/// In-memory audit log with bounded size.
/// In production, these should be persisted to ArangoDB via NATS.
#[derive(Clone)]
pub struct AuditLog {
    entries: Arc<RwLock<Vec<AuditEntry>>>,
    max_entries: usize,
}

impl AuditLog {
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: Arc::new(RwLock::new(Vec::with_capacity(max_entries))),
            max_entries,
        }
    }

    /// Log an audit entry. If the buffer is full, drops the oldest entry.
    pub async fn log(&self, entry: AuditEntry) {
        let mut entries = self.entries.write().await;

        info!(
            action = %entry.action,
            subject = %entry.subject,
            resource = %entry.resource,
            method = %entry.method,
            path = %entry.path,
            status = entry.status,
            "Audit log entry"
        );

        entries.push(entry);

        // Keep only the most recent max_entries
        if entries.len() > self.max_entries {
            entries.remove(0);
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
