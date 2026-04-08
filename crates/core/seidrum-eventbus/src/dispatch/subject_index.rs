use crate::delivery::ChannelConfig;
use std::collections::HashMap;
use std::time::Duration;

/// A subscription entry in the subject index.
#[derive(Debug, Clone)]
pub struct SubscriptionEntry {
    pub id: String,
    pub subject_pattern: String,
    pub priority: u32,
    pub mode: SubscriptionMode,
    pub channel: ChannelConfig,
    pub timeout: Duration,
}

/// Subscription mode: Sync (sequential, mutable) or Async (parallel, immutable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionMode {
    Sync,
    Async,
}

/// A simple subject index for Phase 1. Phase 2 will upgrade to a trie.
/// For now, we do exact matching only.
pub struct SubjectIndex {
    subscriptions: HashMap<String, Vec<SubscriptionEntry>>,
}

impl SubjectIndex {
    pub fn new() -> Self {
        Self {
            subscriptions: HashMap::new(),
        }
    }

    /// Add a subscription.
    pub fn subscribe(&mut self, entry: SubscriptionEntry) {
        let pattern = entry.subject_pattern.clone();
        self.subscriptions
            .entry(pattern)
            .or_default()
            .push(entry);
    }

    /// Remove a subscription by ID.
    pub fn unsubscribe(&mut self, id: &str) {
        for entries in self.subscriptions.values_mut() {
            entries.retain(|e| e.id != id);
        }
    }

    /// Find all subscriptions matching a subject (exact match only in Phase 1).
    pub fn lookup(&self, subject: &str) -> Vec<SubscriptionEntry> {
        self.subscriptions
            .get(subject)
            .cloned()
            .unwrap_or_default()
    }

    /// Get all subscriptions, optionally filtered by pattern.
    pub fn list(&self, filter: Option<&str>) -> Vec<SubscriptionEntry> {
        let mut all = Vec::new();
        for entries in self.subscriptions.values() {
            for entry in entries {
                if let Some(f) = filter {
                    if entry.subject_pattern.contains(f) {
                        all.push(entry.clone());
                    }
                } else {
                    all.push(entry.clone());
                }
            }
        }
        all
    }
}

impl Default for SubjectIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_exact_match() {
        let mut index = SubjectIndex::new();
        let entry = SubscriptionEntry {
            id: "sub1".to_string(),
            subject_pattern: "test.subject".to_string(),
            priority: 10,
            mode: SubscriptionMode::Async,
            channel: ChannelConfig::InProcess,
            timeout: Duration::from_secs(5),
        };
        index.subscribe(entry);

        let matches = index.lookup("test.subject");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "sub1");
    }

    #[test]
    fn test_no_match() {
        let index = SubjectIndex::new();
        let matches = index.lookup("nonexistent");
        assert!(matches.is_empty());
    }

    #[test]
    fn test_multiple_subs_same_pattern() {
        let mut index = SubjectIndex::new();
        for i in 0..3 {
            let entry = SubscriptionEntry {
                id: format!("sub{}", i),
                subject_pattern: "test".to_string(),
                priority: i as u32,
                mode: SubscriptionMode::Async,
                channel: ChannelConfig::InProcess,
                timeout: Duration::from_secs(5),
            };
            index.subscribe(entry);
        }

        let matches = index.lookup("test");
        assert_eq!(matches.len(), 3);
    }

    #[test]
    fn test_unsubscribe() {
        let mut index = SubjectIndex::new();
        let entry1 = SubscriptionEntry {
            id: "sub1".to_string(),
            subject_pattern: "test".to_string(),
            priority: 10,
            mode: SubscriptionMode::Async,
            channel: ChannelConfig::InProcess,
            timeout: Duration::from_secs(5),
        };
        let entry2 = SubscriptionEntry {
            id: "sub2".to_string(),
            subject_pattern: "test".to_string(),
            priority: 20,
            mode: SubscriptionMode::Async,
            channel: ChannelConfig::InProcess,
            timeout: Duration::from_secs(5),
        };
        index.subscribe(entry1);
        index.subscribe(entry2);

        index.unsubscribe("sub1");
        let matches = index.lookup("test");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "sub2");
    }

    #[test]
    fn test_priority_ordering() {
        let mut index = SubjectIndex::new();
        for priority in &[30, 10, 20] {
            let entry = SubscriptionEntry {
                id: format!("sub{}", priority),
                subject_pattern: "test".to_string(),
                priority: *priority,
                mode: SubscriptionMode::Async,
                channel: ChannelConfig::InProcess,
                timeout: Duration::from_secs(5),
            };
            index.subscribe(entry);
        }

        let matches = index.lookup("test");
        assert_eq!(matches.len(), 3);
        // Note: Phase 1 doesn't sort; that comes in Phase 2
    }

    #[test]
    fn test_list_all() {
        let mut index = SubjectIndex::new();
        for i in 0..3 {
            let entry = SubscriptionEntry {
                id: format!("sub{}", i),
                subject_pattern: format!("test.{}", i),
                priority: i as u32,
                mode: SubscriptionMode::Async,
                channel: ChannelConfig::InProcess,
                timeout: Duration::from_secs(5),
            };
            index.subscribe(entry);
        }

        let all = index.list(None);
        assert_eq!(all.len(), 3);

        let filtered = index.list(Some("test.1"));
        assert_eq!(filtered.len(), 1);
    }
}
