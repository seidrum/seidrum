use crate::delivery::ChannelConfig;
use crate::EventBusError;
use std::collections::HashMap;
use std::time::Duration;

/// Maximum nesting depth for subject patterns to prevent stack overflow and DoS attacks.
const MAX_SUBJECT_DEPTH: usize = 256;

/// A subscription entry in the subject index.
#[derive(Debug, Clone)]
pub struct SubscriptionEntry {
    pub id: String,
    pub subject_pattern: String,
    pub priority: u32,
    pub mode: SubscriptionMode,
    pub channel: ChannelConfig,
    pub timeout: Duration,
    pub filter: Option<crate::dispatch::filter::EventFilter>,
}

/// Subscription mode: Sync (sequential, mutable) or Async (parallel, immutable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionMode {
    /// Handler receives the event mutably. Processed sequentially
    /// in priority order. Can modify or drop the event.
    Sync,
    /// Handler receives a clone. Runs in parallel with other
    /// async subscribers. Cannot affect the sync chain.
    Async,
}

/// Trie-based subject index supporting exact match, `*` single-token wildcard,
/// and `>` terminal wildcard. Lookup is O(depth) where depth is the number of
/// tokens in the published subject.
pub struct SubjectIndex {
    root: TrieNode,
}

struct TrieNode {
    /// Subscriptions attached at this exact position.
    subscriptions: Vec<SubscriptionEntry>,
    /// Children keyed by literal token.
    children: HashMap<String, TrieNode>,
    /// Subscriptions using `*` at this position (matches exactly one token).
    wildcard: Option<Box<TrieNode>>,
    /// Subscriptions using `>` at this position (matches one or more trailing tokens).
    terminal_wildcard: Vec<SubscriptionEntry>,
}

impl TrieNode {
    fn new() -> Self {
        Self {
            subscriptions: Vec::new(),
            children: HashMap::new(),
            wildcard: None,
            terminal_wildcard: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.subscriptions.is_empty()
            && self.children.is_empty()
            && self.wildcard.is_none()
            && self.terminal_wildcard.is_empty()
    }
}

impl SubjectIndex {
    pub fn new() -> Self {
        Self {
            root: TrieNode::new(),
        }
    }

    /// Add a subscription. The subject_pattern may contain `*` and `>` tokens.
    /// Returns Err if pattern exceeds maximum depth.
    pub fn subscribe(&mut self, entry: SubscriptionEntry) -> crate::Result<()> {
        let pattern = entry.subject_pattern.clone();
        let tokens: Vec<&str> = pattern.split('.').collect();

        if tokens.len() > MAX_SUBJECT_DEPTH {
            return Err(EventBusError::InvalidSubject(format!(
                "subject pattern exceeds maximum depth of {} tokens",
                MAX_SUBJECT_DEPTH
            )));
        }

        Self::insert(&mut self.root, &tokens, 0, entry);
        Ok(())
    }

    fn insert(node: &mut TrieNode, tokens: &[&str], depth: usize, entry: SubscriptionEntry) {
        if depth == tokens.len() {
            node.subscriptions.push(entry);
            return;
        }

        let token = tokens[depth];

        if token == ">" {
            // Terminal wildcard: matches one or more trailing tokens.
            node.terminal_wildcard.push(entry);
            return;
        }

        if token == "*" {
            // Single-token wildcard: matches exactly one token at this position.
            let child = node
                .wildcard
                .get_or_insert_with(|| Box::new(TrieNode::new()));
            Self::insert(child, tokens, depth + 1, entry);
            return;
        }

        // Literal token.
        let child = node
            .children
            .entry(token.to_string())
            .or_insert_with(TrieNode::new);
        Self::insert(child, tokens, depth + 1, entry);
    }

    /// Remove a subscription by ID from the entire trie.
    pub fn unsubscribe(&mut self, id: &str) {
        Self::remove(&mut self.root, id);
    }

    fn remove(node: &mut TrieNode, id: &str) {
        node.subscriptions.retain(|e| e.id != id);
        node.terminal_wildcard.retain(|e| e.id != id);

        for child in node.children.values_mut() {
            Self::remove(child, id);
        }
        node.children.retain(|_, child| !child.is_empty());

        if let Some(ref mut wildcard) = node.wildcard {
            Self::remove(wildcard, id);
            if wildcard.is_empty() {
                node.wildcard = None;
            }
        }
    }

    /// Find all subscriptions matching a concrete subject (no wildcards in lookup).
    /// Results are sorted by priority (ascending = highest priority first).
    pub fn lookup(&self, subject: &str) -> Vec<SubscriptionEntry> {
        let tokens: Vec<&str> = subject.split('.').collect();
        let mut results = Vec::new();
        Self::collect(&self.root, &tokens, 0, &mut results);
        results.sort_by_key(|e| e.priority);
        results
    }

    fn collect(
        node: &TrieNode,
        tokens: &[&str],
        depth: usize,
        results: &mut Vec<SubscriptionEntry>,
    ) {
        // Terminal wildcard at this node matches any remaining tokens (1 or more).
        if depth < tokens.len() {
            results.extend(node.terminal_wildcard.iter().cloned());
        }

        if depth == tokens.len() {
            results.extend(node.subscriptions.iter().cloned());
            return;
        }

        let token = tokens[depth];

        // Follow literal child.
        if let Some(child) = node.children.get(token) {
            Self::collect(child, tokens, depth + 1, results);
        }

        // Follow `*` wildcard child (matches this one token, then continue).
        if let Some(ref wildcard) = node.wildcard {
            Self::collect(wildcard, tokens, depth + 1, results);
        }
    }

    /// Get all subscriptions, optionally filtered by pattern substring.
    pub fn list(&self, filter: Option<&str>) -> Vec<SubscriptionEntry> {
        let mut all = Vec::new();
        Self::collect_all(&self.root, filter, &mut all);
        all
    }

    fn collect_all(node: &TrieNode, filter: Option<&str>, results: &mut Vec<SubscriptionEntry>) {
        for entry in &node.subscriptions {
            if filter.is_none() || entry.subject_pattern.contains(filter.unwrap_or("")) {
                results.push(entry.clone());
            }
        }
        for entry in &node.terminal_wildcard {
            if filter.is_none() || entry.subject_pattern.contains(filter.unwrap_or("")) {
                results.push(entry.clone());
            }
        }

        for child in node.children.values() {
            Self::collect_all(child, filter, results);
        }
        if let Some(ref wildcard) = node.wildcard {
            Self::collect_all(wildcard, filter, results);
        }
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

    fn make_entry(id: &str, pattern: &str, priority: u32) -> SubscriptionEntry {
        SubscriptionEntry {
            id: id.to_string(),
            subject_pattern: pattern.to_string(),
            priority,
            mode: SubscriptionMode::Async,
            channel: ChannelConfig::InProcess,
            timeout: Duration::from_secs(5),
            filter: None,
        }
    }

    #[test]
    fn test_exact_match() {
        let mut index = SubjectIndex::new();
        index
            .subscribe(make_entry("sub1", "test.subject", 10))
            .unwrap();
        let matches = index.lookup("test.subject");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "sub1");
    }

    #[test]
    fn test_no_match() {
        let index = SubjectIndex::new();
        assert!(index.lookup("nonexistent").is_empty());
    }

    #[test]
    fn test_star_wildcard_single_token() {
        let mut index = SubjectIndex::new();
        index
            .subscribe(make_entry("sub1", "channel.*.inbound", 10))
            .unwrap();
        assert_eq!(index.lookup("channel.telegram.inbound").len(), 1);
        assert_eq!(index.lookup("channel.email.inbound").len(), 1);
    }

    #[test]
    fn test_star_wildcard_no_match_multi_token() {
        let mut index = SubjectIndex::new();
        index
            .subscribe(make_entry("sub1", "channel.*.inbound", 10))
            .unwrap();
        assert!(index.lookup("channel.telegram.sub.inbound").is_empty());
    }

    #[test]
    fn test_star_wildcard_no_match_missing_token() {
        let mut index = SubjectIndex::new();
        index
            .subscribe(make_entry("sub1", "channel.*.inbound", 10))
            .unwrap();
        assert!(index.lookup("channel.inbound").is_empty());
    }

    #[test]
    fn test_gt_wildcard_one_token() {
        let mut index = SubjectIndex::new();
        index.subscribe(make_entry("sub1", "brain.>", 10)).unwrap();
        assert_eq!(index.lookup("brain.content").len(), 1);
    }

    #[test]
    fn test_gt_wildcard_multiple_tokens() {
        let mut index = SubjectIndex::new();
        index.subscribe(make_entry("sub1", "brain.>", 10)).unwrap();
        assert_eq!(index.lookup("brain.content.store").len(), 1);
        assert_eq!(index.lookup("brain.entity.upsert.batch").len(), 1);
    }

    #[test]
    fn test_gt_wildcard_no_match_exact_prefix() {
        let mut index = SubjectIndex::new();
        index.subscribe(make_entry("sub1", "brain.>", 10)).unwrap();
        assert!(index.lookup("brain").is_empty());
    }

    #[test]
    fn test_combined_exact_star_gt() {
        let mut index = SubjectIndex::new();
        index
            .subscribe(make_entry("exact", "channel.telegram.inbound", 10))
            .unwrap();
        index
            .subscribe(make_entry("star", "channel.*.inbound", 20))
            .unwrap();
        index.subscribe(make_entry("gt", "channel.>", 30)).unwrap();

        let matches = index.lookup("channel.telegram.inbound");
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].id, "exact");
        assert_eq!(matches[1].id, "star");
        assert_eq!(matches[2].id, "gt");
    }

    #[test]
    fn test_gt_catches_star_misses() {
        let mut index = SubjectIndex::new();
        index
            .subscribe(make_entry("star", "channel.*.inbound", 10))
            .unwrap();
        index.subscribe(make_entry("gt", "channel.>", 20)).unwrap();
        let matches = index.lookup("channel.telegram.sub.inbound");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "gt");
    }

    #[test]
    fn test_unsubscribe() {
        let mut index = SubjectIndex::new();
        index.subscribe(make_entry("sub1", "test", 10)).unwrap();
        index.subscribe(make_entry("sub2", "test", 20)).unwrap();
        index.unsubscribe("sub1");
        let matches = index.lookup("test");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "sub2");
    }

    #[test]
    fn test_unsubscribe_terminal_wildcard() {
        let mut index = SubjectIndex::new();
        index.subscribe(make_entry("sub1", "brain.>", 10)).unwrap();
        index.unsubscribe("sub1");
        assert!(index.lookup("brain.content.store").is_empty());
    }

    #[test]
    fn test_priority_ordering() {
        let mut index = SubjectIndex::new();
        index.subscribe(make_entry("low", "test", 30)).unwrap();
        index.subscribe(make_entry("high", "test", 10)).unwrap();
        index.subscribe(make_entry("mid", "test", 20)).unwrap();
        let matches = index.lookup("test");
        assert_eq!(matches[0].id, "high");
        assert_eq!(matches[1].id, "mid");
        assert_eq!(matches[2].id, "low");
    }

    #[test]
    fn test_list_all() {
        let mut index = SubjectIndex::new();
        index.subscribe(make_entry("sub0", "test.0", 0)).unwrap();
        index.subscribe(make_entry("sub1", "test.1", 1)).unwrap();
        index.subscribe(make_entry("sub2", "brain.>", 2)).unwrap();
        let all = index.list(None);
        assert_eq!(all.len(), 3);
        let filtered = index.list(Some("test.1"));
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn test_subject_depth_limit() {
        let mut index = SubjectIndex::new();
        let deep_pattern = (0..257)
            .map(|i| format!("level{}", i))
            .collect::<Vec<_>>()
            .join(".");
        let result = index.subscribe(make_entry("sub1", &deep_pattern, 10));
        assert!(result.is_err());
    }
}
