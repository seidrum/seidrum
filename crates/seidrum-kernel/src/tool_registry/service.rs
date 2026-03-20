//! Tool registry service.
//!
//! Subscribes to `tool.register`, `tool.search.request`, and `tool.describe.request`
//! NATS subjects. Persists tools in ArangoDB and maintains an in-memory cache
//! for fast lookups.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::brain::client::ArangoClient;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// In-memory representation of a registered tool.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolEntry {
    pub tool_id: String,
    pub plugin_id: String,
    pub name: String,
    pub summary_md: String,
    pub manual_md: String,
    pub parameters: serde_json::Value,
    pub call_subject: String,
}

/// Registration payload received on `tool.register`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolRegisterPayload {
    pub tool_id: String,
    pub plugin_id: String,
    pub name: String,
    pub summary_md: String,
    pub manual_md: String,
    pub parameters: serde_json::Value,
    pub call_subject: String,
}

/// Confirmation published on `tool.registered`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolRegisteredConfirmation {
    pub tool_id: String,
    pub plugin_id: String,
    pub name: String,
}

/// Request payload for `tool.search.request`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolSearchRequest {
    pub query_text: String,
    pub limit: Option<u32>,
}

/// Summary returned in search results.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolSummary {
    pub tool_id: String,
    pub name: String,
    pub summary_md: String,
    pub parameters: serde_json::Value,
}

/// Response payload for `tool.search.request`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolSearchResponse {
    pub tools: Vec<ToolSummary>,
}

/// Request payload for `tool.describe.request`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolDescribeRequest {
    pub tool_id: String,
}

/// Full tool description returned by `tool.describe.request`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolDescribeResponse {
    pub tool_id: String,
    pub name: String,
    pub summary_md: String,
    pub manual_md: String,
    pub parameters: serde_json::Value,
    pub plugin_id: String,
    pub call_subject: String,
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// Thread-safe in-memory tool registry backed by ArangoDB.
#[derive(Clone)]
pub struct ToolRegistryService {
    /// Map from tool_id to its entry.
    tools: Arc<RwLock<HashMap<String, ToolEntry>>>,
    arango: ArangoClient,
}

impl ToolRegistryService {
    /// Create a new tool registry service.
    pub fn new(arango: ArangoClient) -> Self {
        Self {
            tools: Arc::new(RwLock::new(HashMap::new())),
            arango,
        }
    }

    /// Load all existing tools from ArangoDB into the in-memory cache.
    async fn load_from_db(&self) -> Result<()> {
        let query = r#"
            FOR tool IN tools
                RETURN tool
        "#;
        let resp = self
            .arango
            .execute_aql(query, &serde_json::json!({}))
            .await
            .context("failed to load tools from ArangoDB")?;

        let results = resp
            .get("result")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut tools = self.tools.write().await;
        for doc in results {
            let tool_id = doc
                .get("tool_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if tool_id.is_empty() {
                continue;
            }
            let entry = ToolEntry {
                tool_id: tool_id.clone(),
                plugin_id: doc
                    .get("plugin_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                name: doc
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                summary_md: doc
                    .get("summary_md")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                manual_md: doc
                    .get("manual_md")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                parameters: doc
                    .get("parameters")
                    .cloned()
                    .unwrap_or(serde_json::json!({})),
                call_subject: doc
                    .get("call_subject")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            };
            tools.insert(tool_id, entry);
        }

        info!(count = tools.len(), "loaded tools from ArangoDB into cache");
        Ok(())
    }

    /// Register a tool: persist to ArangoDB, cache in memory, publish confirmation.
    async fn register_tool(
        &self,
        payload: ToolRegisterPayload,
        nats: &async_nats::Client,
    ) -> Result<()> {
        let tool_id = payload.tool_id.clone();
        let plugin_id = payload.plugin_id.clone();
        let name = payload.name.clone();

        // Persist to ArangoDB via UPSERT by tool_id
        let doc = serde_json::json!({
            "tool_id": &payload.tool_id,
            "plugin_id": &payload.plugin_id,
            "name": &payload.name,
            "summary_md": &payload.summary_md,
            "manual_md": &payload.manual_md,
            "parameters": &payload.parameters,
            "call_subject": &payload.call_subject,
        });

        let upsert_query = r#"
            UPSERT { tool_id: @tool_id }
            INSERT MERGE(@doc, { tool_id: @tool_id })
            UPDATE MERGE(OLD, @doc)
            IN tools
            RETURN NEW
        "#;
        self.arango
            .execute_aql(
                upsert_query,
                &serde_json::json!({
                    "tool_id": &payload.tool_id,
                    "doc": &doc,
                }),
            )
            .await
            .with_context(|| format!("failed to upsert tool '{}'", tool_id))?;

        // Cache in memory
        let entry = ToolEntry {
            tool_id: payload.tool_id,
            plugin_id: payload.plugin_id,
            name: payload.name,
            summary_md: payload.summary_md,
            manual_md: payload.manual_md,
            parameters: payload.parameters,
            call_subject: payload.call_subject,
        };

        {
            let mut tools = self.tools.write().await;
            tools.insert(entry.tool_id.clone(), entry);
        }

        // Publish confirmation
        let confirmation = ToolRegisteredConfirmation {
            tool_id: tool_id.clone(),
            plugin_id: plugin_id.clone(),
            name: name.clone(),
        };
        match serde_json::to_vec(&confirmation) {
            Ok(bytes) => {
                if let Err(e) = nats
                    .publish("tool.registered".to_string(), bytes.into())
                    .await
                {
                    warn!(error = %e, "failed to publish tool.registered confirmation");
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to serialize tool.registered confirmation");
            }
        }

        info!(
            tool_id = %tool_id,
            plugin_id = %plugin_id,
            name = %name,
            "tool registered"
        );
        Ok(())
    }

    /// Handle `tool.search.request`: full-text search over summary_md and manual_md.
    /// Falls back to returning all non-meta tools from cache if ArangoSearch returns empty.
    async fn handle_search(&self, req: ToolSearchRequest) -> ToolSearchResponse {
        let limit = req.limit.unwrap_or(5) as usize;
        let query = r#"
            FOR doc IN content_search
                SEARCH ANALYZER(
                    doc.summary_md IN TOKENS(@query_text, "text_en")
                    OR doc.manual_md IN TOKENS(@query_text, "text_en"),
                    "text_en"
                )
                FILTER IS_SAME_COLLECTION("tools", doc)
                SORT BM25(doc) DESC
                LIMIT @limit
                RETURN {
                    tool_id: doc.tool_id,
                    name: doc.name,
                    summary_md: doc.summary_md,
                    parameters: doc.parameters
                }
        "#;

        let mut tools: Vec<ToolSummary> = Vec::new();

        match self
            .arango
            .execute_aql(
                query,
                &serde_json::json!({
                    "query_text": req.query_text,
                    "limit": limit,
                }),
            )
            .await
        {
            Ok(resp) => {
                let results = resp
                    .get("result")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();

                tools = results
                    .into_iter()
                    .filter_map(|v| serde_json::from_value(v).ok())
                    .collect();

                debug!(count = tools.len(), "tool search via ArangoSearch");
            }
            Err(e) => {
                warn!(error = %e, "tool search AQL failed, falling back to cache");
            }
        }

        // Fallback: if ArangoSearch returned nothing, return all non-meta tools from cache
        if tools.is_empty() {
            let cache = self.tools.read().await;
            let meta_ids = ["brain-query", "search-tools", "get-tool-manual"];
            tools = cache
                .values()
                .filter(|t| !meta_ids.contains(&t.tool_id.as_str()))
                .take(limit)
                .map(|t| ToolSummary {
                    tool_id: t.tool_id.clone(),
                    name: t.name.clone(),
                    summary_md: t.summary_md.clone(),
                    parameters: t.parameters.clone(),
                })
                .collect();
            debug!(count = tools.len(), "tool search fallback from cache");
        }

        ToolSearchResponse { tools }
    }

    /// Handle `tool.describe.request`: look up a tool by ID from the in-memory cache.
    async fn handle_describe(&self, req: ToolDescribeRequest) -> Option<ToolDescribeResponse> {
        let tools = self.tools.read().await;
        tools.get(&req.tool_id).map(|entry| ToolDescribeResponse {
            tool_id: entry.tool_id.clone(),
            name: entry.name.clone(),
            summary_md: entry.summary_md.clone(),
            manual_md: entry.manual_md.clone(),
            parameters: entry.parameters.clone(),
            plugin_id: entry.plugin_id.clone(),
            call_subject: entry.call_subject.clone(),
        })
    }

    /// Get a tool by ID from the in-memory cache.
    pub async fn get_tool(&self, tool_id: &str) -> Option<ToolEntry> {
        let tools = self.tools.read().await;
        tools.get(tool_id).cloned()
    }

    /// List all registered tools.
    pub async fn list_tools(&self) -> Vec<ToolEntry> {
        let tools = self.tools.read().await;
        tools.values().cloned().collect()
    }

    /// Register the built-in meta-tools that the kernel itself provides.
    async fn register_meta_tools(&self, nats: &async_nats::Client) -> Result<()> {
        let meta_tools = vec![
            ToolRegisterPayload {
                tool_id: "brain-query".to_string(),
                plugin_id: "kernel".to_string(),
                name: "Brain Query".to_string(),
                summary_md: "Query the knowledge graph using AQL".to_string(),
                manual_md: concat!(
                    "# brain-query\n\n",
                    "Execute an AQL query against the Seidrum knowledge graph (ArangoDB).\n\n",
                    "## Parameters\n\n",
                    "- `query_type` (string, required): One of `aql`, `vector_search`, ",
                    "`graph_traverse`, `get_facts`, `get_context`.\n",
                    "- `aql` (string): Raw AQL query string (for `aql` type).\n",
                    "- `bind_vars` (object): AQL bind variables.\n",
                    "- `limit` (integer): Max results to return.\n",
                )
                .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query_type": { "type": "string", "enum": ["aql", "vector_search", "graph_traverse", "get_facts", "get_context"] },
                        "aql": { "type": "string" },
                        "bind_vars": { "type": "object" },
                        "limit": { "type": "integer" }
                    },
                    "required": ["query_type"]
                }),
                call_subject: "tool.call.kernel".to_string(),
            },
            ToolRegisterPayload {
                tool_id: "search-tools".to_string(),
                plugin_id: "kernel".to_string(),
                name: "Search Tools".to_string(),
                summary_md: "Search for available tools by description".to_string(),
                manual_md: concat!(
                    "# search-tools\n\n",
                    "Full-text search across all registered tool summaries and manuals.\n\n",
                    "## Parameters\n\n",
                    "- `query_text` (string, required): Natural-language search query.\n",
                    "- `limit` (integer): Max results (default 5).\n",
                )
                .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query_text": { "type": "string" },
                        "limit": { "type": "integer" }
                    },
                    "required": ["query_text"]
                }),
                call_subject: "tool.search.request".to_string(),
            },
            ToolRegisterPayload {
                tool_id: "get-tool-manual".to_string(),
                plugin_id: "kernel".to_string(),
                name: "Get Tool Manual".to_string(),
                summary_md: "Get the full manual for a tool by ID".to_string(),
                manual_md: concat!(
                    "# get-tool-manual\n\n",
                    "Retrieve the full documentation (manual_md) for a specific tool.\n\n",
                    "## Parameters\n\n",
                    "- `tool_id` (string, required): The unique tool identifier.\n",
                )
                .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "tool_id": { "type": "string" }
                    },
                    "required": ["tool_id"]
                }),
                call_subject: "tool.describe.request".to_string(),
            },
        ];

        for tool in meta_tools {
            self.register_tool(tool, nats).await?;
        }

        info!("meta-tools registered (brain-query, search-tools, get-tool-manual)");
        Ok(())
    }

    /// Spawn the tool registry service background tasks.
    ///
    /// Subscribes to:
    /// - `tool.register` — for tool registration events
    /// - `tool.search.request` — for full-text search (request/reply)
    /// - `tool.describe.request` — for tool description lookup (request/reply)
    ///
    /// Returns a `JoinHandle` for the spawned task.
    pub async fn spawn(
        self,
        nats_client: async_nats::Client,
    ) -> Result<tokio::task::JoinHandle<()>> {
        // Load existing tools from the database.
        if let Err(e) = self.load_from_db().await {
            warn!(error = %e, "failed to load tools from ArangoDB (may not be initialized yet)");
        }

        // Register meta-tools.
        if let Err(e) = self.register_meta_tools(&nats_client).await {
            warn!(error = %e, "failed to register meta-tools");
        }

        let mut register_sub = nats_client
            .subscribe("tool.register".to_string())
            .await
            .context("failed to subscribe to tool.register")?;
        info!("tool_registry: subscribed to tool.register");

        let mut search_sub = nats_client
            .subscribe("tool.search.request".to_string())
            .await
            .context("failed to subscribe to tool.search.request")?;
        info!("tool_registry: subscribed to tool.search.request");

        let mut describe_sub = nats_client
            .subscribe("tool.describe.request".to_string())
            .await
            .context("failed to subscribe to tool.describe.request")?;
        info!("tool_registry: subscribed to tool.describe.request");

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(msg) = register_sub.next() => {
                        match serde_json::from_slice::<ToolRegisterPayload>(&msg.payload) {
                            Ok(payload) => {
                                if let Err(e) = self.register_tool(payload, &nats_client).await {
                                    error!(error = %e, "tool registration failed");
                                }
                            }
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    "failed to deserialize tool.register payload"
                                );
                            }
                        }
                    }
                    Some(msg) = search_sub.next() => {
                        match serde_json::from_slice::<ToolSearchRequest>(&msg.payload) {
                            Ok(req) => {
                                let response = self.handle_search(req).await;
                                if let Some(reply) = msg.reply {
                                    match serde_json::to_vec(&response) {
                                        Ok(bytes) => {
                                            if let Err(e) = nats_client
                                                .publish(reply, bytes.into())
                                                .await
                                            {
                                                warn!(
                                                    error = %e,
                                                    "failed to publish tool.search.request response"
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            warn!(
                                                error = %e,
                                                "failed to serialize tool.search response"
                                            );
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    "failed to deserialize tool.search.request payload"
                                );
                                if let Some(reply) = msg.reply {
                                    let err_resp = ToolSearchResponse { tools: vec![] };
                                    if let Ok(bytes) = serde_json::to_vec(&err_resp) {
                                        let _ = nats_client.publish(reply, bytes.into()).await;
                                    }
                                }
                            }
                        }
                    }
                    Some(msg) = describe_sub.next() => {
                        match serde_json::from_slice::<ToolDescribeRequest>(&msg.payload) {
                            Ok(req) => {
                                let response = self.handle_describe(req).await;
                                if let Some(reply) = msg.reply {
                                    match serde_json::to_vec(&response) {
                                        Ok(bytes) => {
                                            if let Err(e) = nats_client
                                                .publish(reply, bytes.into())
                                                .await
                                            {
                                                warn!(
                                                    error = %e,
                                                    "failed to publish tool.describe.request response"
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            warn!(
                                                error = %e,
                                                "failed to serialize tool.describe response"
                                            );
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    "failed to deserialize tool.describe.request payload"
                                );
                                if let Some(reply) = msg.reply {
                                    let err_resp: Option<ToolDescribeResponse> = None;
                                    if let Ok(bytes) = serde_json::to_vec(&err_resp) {
                                        let _ = nats_client.publish(reply, bytes.into()).await;
                                    }
                                }
                            }
                        }
                    }
                    else => break,
                }
            }
            info!("tool registry service stopped");
        });

        Ok(handle)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a ToolRegistryService with a dummy ArangoClient for unit tests
    /// that only exercise the in-memory cache (no DB calls).
    fn make_test_service() -> ToolRegistryService {
        // The ArangoClient will never be called in these tests, but we need
        // a valid instance to construct the service. Use a dummy URL.
        let arango =
            ArangoClient::new("http://localhost:99999", "test_db", "").expect("dummy client");
        ToolRegistryService::new(arango)
    }

    fn sample_entry(tool_id: &str, plugin_id: &str, name: &str) -> ToolEntry {
        ToolEntry {
            tool_id: tool_id.to_string(),
            plugin_id: plugin_id.to_string(),
            name: name.to_string(),
            summary_md: format!("Summary for {}", name),
            manual_md: format!("# {}\n\nManual content.", name),
            parameters: serde_json::json!({"type": "object"}),
            call_subject: format!("tool.call.{}", plugin_id),
        }
    }

    #[tokio::test]
    async fn register_and_lookup() {
        let svc = make_test_service();

        // Insert directly into cache (bypassing ArangoDB for unit test)
        let entry = sample_entry("brain-query", "kernel", "Brain Query");
        {
            let mut tools = svc.tools.write().await;
            tools.insert(entry.tool_id.clone(), entry);
        }

        let found = svc.get_tool("brain-query").await;
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.tool_id, "brain-query");
        assert_eq!(found.plugin_id, "kernel");
        assert_eq!(found.name, "Brain Query");

        // Not found
        assert!(svc.get_tool("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn list_tools_returns_all() {
        let svc = make_test_service();

        {
            let mut tools = svc.tools.write().await;
            tools.insert(
                "tool-a".to_string(),
                sample_entry("tool-a", "plugin-a", "Tool A"),
            );
            tools.insert(
                "tool-b".to_string(),
                sample_entry("tool-b", "plugin-b", "Tool B"),
            );
        }

        let all = svc.list_tools().await;
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn describe_returns_full_entry() {
        let svc = make_test_service();

        let entry = sample_entry("my-tool", "my-plugin", "My Tool");
        {
            let mut tools = svc.tools.write().await;
            tools.insert(entry.tool_id.clone(), entry);
        }

        let req = ToolDescribeRequest {
            tool_id: "my-tool".to_string(),
        };
        let resp = svc.handle_describe(req).await;
        assert!(resp.is_some());
        let resp = resp.unwrap();
        assert_eq!(resp.tool_id, "my-tool");
        assert_eq!(resp.plugin_id, "my-plugin");
        assert_eq!(resp.name, "My Tool");
        assert!(resp.manual_md.contains("Manual content"));
        assert_eq!(resp.call_subject, "tool.call.my-plugin");

        // Missing tool
        let req = ToolDescribeRequest {
            tool_id: "missing".to_string(),
        };
        assert!(svc.handle_describe(req).await.is_none());
    }

    #[tokio::test]
    async fn overwrite_existing_tool() {
        let svc = make_test_service();

        {
            let mut tools = svc.tools.write().await;
            tools.insert(
                "t1".to_string(),
                sample_entry("t1", "p1", "Version 1"),
            );
        }

        // Overwrite with new data
        {
            let mut tools = svc.tools.write().await;
            tools.insert(
                "t1".to_string(),
                sample_entry("t1", "p1", "Version 2"),
            );
        }

        let found = svc.get_tool("t1").await.unwrap();
        assert_eq!(found.name, "Version 2");
        assert_eq!(svc.list_tools().await.len(), 1);
    }

    #[test]
    fn tool_entry_serialization_roundtrip() {
        let entry = ToolEntry {
            tool_id: "brain-query".to_string(),
            plugin_id: "kernel".to_string(),
            name: "Brain Query".to_string(),
            summary_md: "Query the knowledge graph".to_string(),
            manual_md: "# Brain Query\n\nFull manual.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query_type": { "type": "string" }
                }
            }),
            call_subject: "tool.call.kernel".to_string(),
        };

        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: ToolEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry.tool_id, deserialized.tool_id);
        assert_eq!(entry.plugin_id, deserialized.plugin_id);
        assert_eq!(entry.name, deserialized.name);
        assert_eq!(entry.summary_md, deserialized.summary_md);
        assert_eq!(entry.call_subject, deserialized.call_subject);
    }

    #[test]
    fn search_response_serialization_roundtrip() {
        let response = ToolSearchResponse {
            tools: vec![
                ToolSummary {
                    tool_id: "brain-query".to_string(),
                    name: "Brain Query".to_string(),
                    summary_md: "Query the knowledge graph".to_string(),
                    parameters: serde_json::json!({"type": "object"}),
                },
                ToolSummary {
                    tool_id: "search-tools".to_string(),
                    name: "Search Tools".to_string(),
                    summary_md: "Search for available tools".to_string(),
                    parameters: serde_json::json!({"type": "object"}),
                },
            ],
        };

        let json = serde_json::to_string(&response).unwrap();
        let deserialized: ToolSearchResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.tools.len(), 2);
        assert_eq!(deserialized.tools[0].tool_id, "brain-query");
        assert_eq!(deserialized.tools[1].tool_id, "search-tools");
    }

    #[test]
    fn describe_response_serialization_roundtrip() {
        let response = ToolDescribeResponse {
            tool_id: "brain-query".to_string(),
            name: "Brain Query".to_string(),
            summary_md: "Query the knowledge graph".to_string(),
            manual_md: "# Brain Query\n\nDetailed docs.".to_string(),
            parameters: serde_json::json!({"type": "object"}),
            plugin_id: "kernel".to_string(),
            call_subject: "tool.call.kernel".to_string(),
        };

        let json = serde_json::to_string(&response).unwrap();
        let deserialized: ToolDescribeResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.tool_id, "brain-query");
        assert_eq!(deserialized.manual_md, "# Brain Query\n\nDetailed docs.");
        assert_eq!(deserialized.call_subject, "tool.call.kernel");
    }
}
