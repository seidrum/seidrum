//! Scope resolution and AQL filter injection for scope enforcement.
//!
//! The kernel enforces scopes on every brain query. This module provides:
//!
//! 1. **Scope resolution** — given an agent's primary scope and additional
//!    scopes, resolve the full set of accessible scopes by traversing the
//!    scope hierarchy upward (parent scopes) and reading cross-scope read
//!    permissions from `access_rules`.
//!
//! 2. **AQL filter injection** — given a set of allowed scopes, wrap or
//!    modify an AQL query so that only results scoped to one of those
//!    scopes are returned. This works via the `scoped_to` edge collection.

use std::collections::HashSet;

use anyhow::{Context, Result};
use serde_json::Value;
use tracing::{debug, warn};

use crate::brain::client::ArangoClient;

/// Scope enforcement service. Resolves accessible scopes and injects
/// scope filters into AQL queries.
#[derive(Clone)]
pub struct ScopeService {
    arango: ArangoClient,
}

/// The resolved set of scopes an agent may access.
#[derive(Debug, Clone)]
pub struct ResolvedScopes {
    /// All scope keys the agent is allowed to read from.
    pub allowed: HashSet<String>,
    /// The primary scope key.
    #[allow(dead_code)]
    pub primary: String,
}

impl ScopeService {
    /// Create a new scope service backed by the given ArangoDB client.
    pub fn new(arango: ArangoClient) -> Self {
        Self { arango }
    }

    // ------------------------------------------------------------------
    // Scope resolution
    // ------------------------------------------------------------------

    /// Resolve the full set of accessible scopes for an agent.
    ///
    /// Starting from the agent's `primary_scope` plus `additional_scopes`,
    /// this method:
    /// 1. Adds parent scopes (walking `parent_scope` upward to the root).
    /// 2. Adds any scopes listed in the scope's `access_rules.cross_scope_read`.
    /// 3. Always includes `scope_root` (identity scope — always accessible).
    pub async fn resolve_scopes(
        &self,
        primary_scope: &str,
        additional_scopes: &[String],
    ) -> Result<ResolvedScopes> {
        let mut allowed: HashSet<String> = HashSet::new();

        // Seed with the declared scopes.
        let mut to_visit: Vec<String> = Vec::new();
        to_visit.push(primary_scope.to_string());
        for s in additional_scopes {
            to_visit.push(s.clone());
        }

        // Walk each seed scope: fetch the document, add parent + cross-scope reads.
        let mut visited: HashSet<String> = HashSet::new();
        while let Some(scope_key) = to_visit.pop() {
            if visited.contains(&scope_key) {
                continue;
            }
            visited.insert(scope_key.clone());
            allowed.insert(scope_key.clone());

            // Fetch the scope document from ArangoDB.
            match self.arango.get_document("scopes", &scope_key).await {
                Ok(Some(doc)) => {
                    // Traverse parent_scope upward.
                    if let Some(parent) = doc.get("parent_scope").and_then(|v| v.as_str()) {
                        if !parent.is_empty() {
                            to_visit.push(parent.to_string());
                        }
                    }

                    // Add cross_scope_read entries.
                    if let Some(access_rules) = doc.get("access_rules") {
                        if let Some(cross_read) = access_rules.get("cross_scope_read") {
                            if let Some(arr) = cross_read.as_array() {
                                for entry in arr {
                                    if let Some(s) = entry.as_str() {
                                        to_visit.push(s.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(None) => {
                    warn!(scope_key, "scope document not found in ArangoDB");
                }
                Err(e) => {
                    warn!(scope_key, error = %e, "failed to fetch scope document");
                }
            }
        }

        // scope_root (identity) is always accessible.
        allowed.insert("scope_root".to_string());

        debug!(
            primary = primary_scope,
            count = allowed.len(),
            scopes = ?allowed,
            "resolved accessible scopes"
        );

        Ok(ResolvedScopes {
            allowed,
            primary: primary_scope.to_string(),
        })
    }

    // ------------------------------------------------------------------
    // AQL filter injection
    // ------------------------------------------------------------------

    /// Inject a scope filter into an AQL query.
    ///
    /// This wraps the original query so that only documents that have a
    /// `scoped_to` edge pointing to one of the allowed scopes are returned.
    ///
    /// The approach:
    /// - The original query is executed as a subquery.
    /// - Each result document is checked: does it have a `scoped_to` edge
    ///   to any of the allowed scopes?
    /// - Documents without any `scoped_to` edge are *included* by default
    ///   (unscoped data is globally visible).
    ///
    /// Returns `(wrapped_aql, merged_bind_vars)`.
    pub fn inject_scope_filter(
        &self,
        aql: &str,
        bind_vars: &Value,
        allowed_scopes: &HashSet<String>,
    ) -> (String, Value) {
        if allowed_scopes.is_empty() {
            // No scopes resolved — pass through unfiltered (should not happen).
            return (aql.to_string(), bind_vars.clone());
        }

        // Build the list of allowed scope IDs (with `scopes/` prefix).
        let scope_ids: Vec<String> = allowed_scopes
            .iter()
            .map(|s| {
                if s.contains('/') {
                    s.clone()
                } else {
                    format!("scopes/{}", s)
                }
            })
            .collect();

        // The wrapped query:
        // 1. Run the original query as a subquery to get candidate documents.
        // 2. For each candidate, check if it has a scoped_to edge to any of
        //    the allowed scopes, OR if it has no scoped_to edges at all
        //    (unscoped items are globally visible).
        let wrapped = format!(
            r#"LET __scope_allowed = @__scope_allowed_scopes
LET __scope_candidates = (
{original}
)
FOR __scope_doc IN __scope_candidates
    LET __scope_edges = (
        FOR __se IN scoped_to
            FILTER __se._from == __scope_doc._id
            RETURN __se._to
    )
    FILTER LENGTH(__scope_edges) == 0
        OR LENGTH(INTERSECTION(__scope_edges, __scope_allowed)) > 0
    RETURN __scope_doc"#,
            original = aql.trim().trim_end_matches(';'),
        );

        // Merge the scope bind var into the existing bind vars.
        let mut merged = bind_vars.clone();
        if let Some(obj) = merged.as_object_mut() {
            obj.insert(
                "__scope_allowed_scopes".to_string(),
                serde_json::json!(scope_ids),
            );
        } else {
            merged = serde_json::json!({
                "__scope_allowed_scopes": scope_ids,
            });
        }

        (wrapped, merged)
    }

    /// Convenience: resolve scopes for an agent and inject a filter into
    /// an AQL query in one step.
    #[allow(dead_code)]
    pub async fn enforce_scope_on_aql(
        &self,
        primary_scope: &str,
        additional_scopes: &[String],
        aql: &str,
        bind_vars: &Value,
    ) -> Result<(String, Value)> {
        let resolved = self
            .resolve_scopes(primary_scope, additional_scopes)
            .await
            .context("failed to resolve scopes")?;

        Ok(self.inject_scope_filter(aql, bind_vars, &resolved.allowed))
    }

    /// Build a scope-filtered AQL clause that can be used inside existing
    /// queries as an additional FILTER. This is lighter-weight than wrapping
    /// the entire query — useful when you control the query structure.
    ///
    /// Returns an AQL fragment like:
    /// ```aql
    /// LET __scoped_ids = (
    ///     FOR __s IN scoped_to
    ///         FILTER __s._to IN @__scope_allowed_scopes
    ///         RETURN __s._from
    /// )
    /// ```
    ///
    /// Callers can then add `FILTER doc._id IN __scoped_ids OR doc._id NOT IN (FOR s IN scoped_to RETURN s._from)`
    /// to their query.
    #[allow(dead_code)]
    pub fn scope_filter_fragment(allowed_scopes: &HashSet<String>) -> (String, Value) {
        let scope_ids: Vec<String> = allowed_scopes
            .iter()
            .map(|s| {
                if s.contains('/') {
                    s.clone()
                } else {
                    format!("scopes/{}", s)
                }
            })
            .collect();

        let fragment = r#"LET __scoped_ids = (
    FOR __s IN scoped_to
        FILTER __s._to IN @__scope_allowed_scopes
        RETURN __s._from
)
LET __all_scoped = (
    FOR __s IN scoped_to
        RETURN DISTINCT __s._from
)"#
        .to_string();

        let bind = serde_json::json!({
            "__scope_allowed_scopes": scope_ids,
        });

        (fragment, bind)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // We cannot unit-test resolve_scopes without an ArangoDB instance,
    // but we can thoroughly test the AQL filter injection logic.

    /// Helper to create a ScopeService with a dummy client (will not be used
    /// in filter-only tests).
    fn dummy_scope_service() -> ScopeService {
        // The ArangoClient::new will succeed with any params — it just builds
        // an HTTP client. We will never actually call ArangoDB in these tests.
        let client = ArangoClient::new("http://localhost:9999", "test", "").unwrap();
        ScopeService::new(client)
    }

    #[test]
    fn test_inject_scope_filter_wraps_query() {
        let svc = dummy_scope_service();
        let mut scopes = HashSet::new();
        scopes.insert("scope_root".to_string());
        scopes.insert("scope_career".to_string());

        let original_aql = r#"FOR doc IN entities RETURN doc"#;
        let bind_vars = serde_json::json!({});

        let (wrapped, merged_vars) = svc.inject_scope_filter(original_aql, &bind_vars, &scopes);

        // The wrapped query should contain the original query.
        assert!(
            wrapped.contains("FOR doc IN entities RETURN doc"),
            "wrapped query must contain original AQL"
        );

        // It should reference the scope filter constructs.
        assert!(
            wrapped.contains("__scope_allowed"),
            "wrapped query must reference scope allowed list"
        );
        assert!(
            wrapped.contains("scoped_to"),
            "wrapped query must reference scoped_to edge collection"
        );
        assert!(
            wrapped.contains("__scope_candidates"),
            "wrapped query must use __scope_candidates subquery"
        );

        // The bind vars should include the scope list.
        let scope_var = merged_vars
            .get("__scope_allowed_scopes")
            .expect("must have __scope_allowed_scopes bind var");
        let scope_arr = scope_var.as_array().expect("must be an array");
        assert_eq!(scope_arr.len(), 2);

        // All entries should be prefixed with "scopes/".
        for entry in scope_arr {
            let s = entry.as_str().unwrap();
            assert!(
                s.starts_with("scopes/"),
                "scope ID '{}' should start with 'scopes/'",
                s
            );
        }
    }

    #[test]
    fn test_inject_scope_filter_preserves_existing_bind_vars() {
        let svc = dummy_scope_service();
        let mut scopes = HashSet::new();
        scopes.insert("scope_projects".to_string());

        let original_aql = r#"FOR doc IN entities FILTER doc.type == @type RETURN doc"#;
        let bind_vars = serde_json::json!({
            "type": "person",
        });

        let (_wrapped, merged_vars) = svc.inject_scope_filter(original_aql, &bind_vars, &scopes);

        // Original bind vars must still be present.
        assert_eq!(
            merged_vars.get("type").and_then(|v| v.as_str()),
            Some("person"),
            "original bind var 'type' must be preserved"
        );

        // Scope bind var must also be present.
        assert!(
            merged_vars.get("__scope_allowed_scopes").is_some(),
            "scope bind var must be added"
        );
    }

    #[test]
    fn test_inject_scope_filter_empty_scopes_passthrough() {
        let svc = dummy_scope_service();
        let scopes: HashSet<String> = HashSet::new();

        let original = "FOR doc IN entities RETURN doc";
        let vars = serde_json::json!({});

        let (result_aql, result_vars) = svc.inject_scope_filter(original, &vars, &scopes);

        // With empty scopes, query should pass through unchanged.
        assert_eq!(result_aql, original);
        assert_eq!(result_vars, vars);
    }

    #[test]
    fn test_inject_scope_filter_handles_semicolons() {
        let svc = dummy_scope_service();
        let mut scopes = HashSet::new();
        scopes.insert("scope_root".to_string());

        let original = "FOR doc IN entities RETURN doc;";
        let vars = serde_json::json!({});

        let (wrapped, _) = svc.inject_scope_filter(original, &vars, &scopes);

        // The trailing semicolon should be stripped from the subquery.
        assert!(
            !wrapped.contains("RETURN doc;"),
            "trailing semicolon should be stripped in the subquery"
        );
    }

    #[test]
    fn test_inject_scope_filter_scope_ids_are_normalized() {
        let svc = dummy_scope_service();
        let mut scopes = HashSet::new();
        // Mix: one with prefix, one without.
        scopes.insert("scope_root".to_string());
        scopes.insert("scopes/scope_custom".to_string());

        let original = "FOR doc IN entities RETURN doc";
        let vars = serde_json::json!({});

        let (_, merged) = svc.inject_scope_filter(original, &vars, &scopes);

        let arr = merged
            .get("__scope_allowed_scopes")
            .unwrap()
            .as_array()
            .unwrap();

        // All should start with "scopes/".
        for entry in arr {
            let s = entry.as_str().unwrap();
            assert!(s.starts_with("scopes/"), "expected scopes/ prefix: {}", s);
        }

        // The one that already had "scopes/" should not be double-prefixed.
        let has_double = arr
            .iter()
            .any(|v| v.as_str().unwrap().starts_with("scopes/scopes/"));
        assert!(!has_double, "should not have double scopes/ prefix");
    }

    #[test]
    fn test_scope_filter_fragment_produces_valid_structure() {
        let mut scopes = HashSet::new();
        scopes.insert("scope_root".to_string());
        scopes.insert("scope_career".to_string());

        let (fragment, bind) = ScopeService::scope_filter_fragment(&scopes);

        assert!(fragment.contains("__scoped_ids"));
        assert!(fragment.contains("scoped_to"));
        assert!(fragment.contains("@__scope_allowed_scopes"));

        let arr = bind
            .get("__scope_allowed_scopes")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn test_inject_scope_filter_includes_unscoped_documents() {
        let svc = dummy_scope_service();
        let mut scopes = HashSet::new();
        scopes.insert("scope_root".to_string());

        let original = "FOR doc IN entities RETURN doc";
        let vars = serde_json::json!({});

        let (wrapped, _) = svc.inject_scope_filter(original, &vars, &scopes);

        // The wrapped query should allow documents with no scoped_to edges.
        assert!(
            wrapped.contains("LENGTH(__scope_edges) == 0"),
            "should include unscoped documents (no scoped_to edges)"
        );
    }

    #[test]
    fn test_resolved_scopes_always_includes_root() {
        // This tests the data structure, not the async resolution.
        let resolved = ResolvedScopes {
            allowed: {
                let mut s = HashSet::new();
                s.insert("scope_root".to_string());
                s.insert("scope_career".to_string());
                s
            },
            primary: "scope_career".to_string(),
        };

        assert!(resolved.allowed.contains("scope_root"));
        assert!(resolved.allowed.contains("scope_career"));
        assert_eq!(resolved.primary, "scope_career");
    }
}
