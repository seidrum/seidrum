use serde::{Deserialize, Serialize};

/// An event filter narrows which events a subscription receives beyond
/// subject matching. Filters operate on the serialized JSON payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventFilter {
    /// Match a top-level JSON field against an exact value.
    FieldEquals {
        path: String,
        value: serde_json::Value,
    },
    /// Match a top-level JSON field containing a substring.
    FieldContains { path: String, substring: String },
    /// All sub-filters must match (logical AND).
    All(Vec<EventFilter>),
    /// Any sub-filter must match (logical OR).
    Any(Vec<EventFilter>),
}

impl EventFilter {
    /// Evaluate the filter against a raw payload (bytes).
    /// Returns true if the payload matches the filter or if the payload
    /// is not valid JSON (filters are best-effort; non-JSON payloads pass through).
    pub fn matches(&self, payload: &[u8]) -> bool {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(payload) else {
            // Non-JSON payloads pass through filters (filter is a narrowing hint,
            // not a hard gate — the subscriber decides how to handle non-JSON).
            return true;
        };
        self.matches_value(&value)
    }

    fn matches_value(&self, root: &serde_json::Value) -> bool {
        match self {
            EventFilter::FieldEquals { path, value } => {
                Self::resolve_path(root, path).map_or(false, |v| v == value)
            }
            EventFilter::FieldContains { path, substring } => Self::resolve_path(root, path)
                .map_or(false, |v| match v {
                    serde_json::Value::String(s) => s.contains(substring.as_str()),
                    other => other.to_string().contains(substring.as_str()),
                }),
            EventFilter::All(filters) => filters.iter().all(|f| f.matches_value(root)),
            EventFilter::Any(filters) => filters.iter().any(|f| f.matches_value(root)),
        }
    }

    /// Simple dot-separated path resolution on a JSON value.
    /// Supports `"field"` for top-level and `"field.nested"` for nested access.
    fn resolve_path<'a>(root: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
        let mut current = root;
        for segment in path.split('.') {
            current = current.get(segment)?;
        }
        Some(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_field_equals_match() {
        let filter = EventFilter::FieldEquals {
            path: "platform".to_string(),
            value: json!("telegram"),
        };
        let payload = serde_json::to_vec(&json!({"platform": "telegram", "text": "hi"})).unwrap();
        assert!(filter.matches(&payload));
    }

    #[test]
    fn test_field_equals_no_match() {
        let filter = EventFilter::FieldEquals {
            path: "platform".to_string(),
            value: json!("telegram"),
        };
        let payload = serde_json::to_vec(&json!({"platform": "email", "text": "hi"})).unwrap();
        assert!(!filter.matches(&payload));
    }

    #[test]
    fn test_field_equals_missing_field() {
        let filter = EventFilter::FieldEquals {
            path: "platform".to_string(),
            value: json!("telegram"),
        };
        let payload = serde_json::to_vec(&json!({"text": "hi"})).unwrap();
        assert!(!filter.matches(&payload));
    }

    #[test]
    fn test_field_contains() {
        let filter = EventFilter::FieldContains {
            path: "text".to_string(),
            substring: "hello".to_string(),
        };
        let payload = serde_json::to_vec(&json!({"text": "say hello world"})).unwrap();
        assert!(filter.matches(&payload));
    }

    #[test]
    fn test_field_contains_no_match() {
        let filter = EventFilter::FieldContains {
            path: "text".to_string(),
            substring: "goodbye".to_string(),
        };
        let payload = serde_json::to_vec(&json!({"text": "say hello world"})).unwrap();
        assert!(!filter.matches(&payload));
    }

    #[test]
    fn test_all_filter() {
        let filter = EventFilter::All(vec![
            EventFilter::FieldEquals {
                path: "platform".to_string(),
                value: json!("telegram"),
            },
            EventFilter::FieldContains {
                path: "text".to_string(),
                substring: "hello".to_string(),
            },
        ]);
        let payload =
            serde_json::to_vec(&json!({"platform": "telegram", "text": "hello world"})).unwrap();
        assert!(filter.matches(&payload));

        let payload =
            serde_json::to_vec(&json!({"platform": "email", "text": "hello world"})).unwrap();
        assert!(!filter.matches(&payload));
    }

    #[test]
    fn test_any_filter() {
        let filter = EventFilter::Any(vec![
            EventFilter::FieldEquals {
                path: "platform".to_string(),
                value: json!("telegram"),
            },
            EventFilter::FieldEquals {
                path: "platform".to_string(),
                value: json!("email"),
            },
        ]);
        let payload = serde_json::to_vec(&json!({"platform": "email"})).unwrap();
        assert!(filter.matches(&payload));

        let payload = serde_json::to_vec(&json!({"platform": "sms"})).unwrap();
        assert!(!filter.matches(&payload));
    }

    #[test]
    fn test_nested_path() {
        let filter = EventFilter::FieldEquals {
            path: "user.id".to_string(),
            value: json!(42),
        };
        let payload = serde_json::to_vec(&json!({"user": {"id": 42, "name": "alice"}})).unwrap();
        assert!(filter.matches(&payload));
    }

    #[test]
    fn test_non_json_payload_passes() {
        let filter = EventFilter::FieldEquals {
            path: "platform".to_string(),
            value: json!("telegram"),
        };
        // Raw bytes that aren't valid JSON
        assert!(filter.matches(b"not json at all"));
    }

    #[test]
    fn test_empty_all_matches() {
        let filter = EventFilter::All(vec![]);
        let payload = serde_json::to_vec(&json!({"anything": true})).unwrap();
        assert!(filter.matches(&payload));
    }

    #[test]
    fn test_empty_any_does_not_match() {
        let filter = EventFilter::Any(vec![]);
        let payload = serde_json::to_vec(&json!({"anything": true})).unwrap();
        assert!(!filter.matches(&payload));
    }
}
