//! ArangoDB HTTP client wrapper.
//!
//! Uses `reqwest` to call the ArangoDB REST API directly, avoiding
//! the `arangors` crate which can be finicky.

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;
use tracing::{debug, trace};

/// Lightweight wrapper around the ArangoDB HTTP API.
#[derive(Clone)]
pub struct ArangoClient {
    /// Base URL, e.g. `http://localhost:8529`
    base_url: String,
    /// Database name, e.g. `seidrum`
    database: String,
    /// HTTP client with connection pooling.
    http: Client,
    /// Basic-auth header value (`root:<password>`).
    auth_header: String,
}

impl ArangoClient {
    /// Create a new client.
    ///
    /// * `base_url` — ArangoDB server URL (no trailing slash).
    /// * `database` — target database name.
    /// * `password` — root password (empty string if none).
    pub fn new(base_url: &str, database: &str, password: &str) -> Result<Self> {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("failed to build HTTP client")?;

        // ArangoDB uses HTTP basic auth; default user is "root".
        use base64::Engine as _;
        let credentials = format!("root:{}", password);
        let encoded = base64::engine::general_purpose::STANDARD.encode(credentials.as_bytes());
        let auth_header = format!("Basic {}", encoded);

        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            database: database.to_string(),
            http,
            auth_header,
        })
    }

    // ------------------------------------------------------------------
    // Low-level helpers
    // ------------------------------------------------------------------

    /// Build a URL against the _system database (e.g. for creating databases).
    fn system_url(&self, path: &str) -> String {
        format!("{}/_db/_system{}", self.base_url, path)
    }

    /// Build a URL scoped to the target database.
    fn db_url(&self, path: &str) -> String {
        format!("{}/_db/{}{}", self.base_url, self.database, path)
    }

    /// POST with JSON body and return the response body as `Value`.
    pub async fn post(&self, url: &str, body: &Value) -> Result<Value> {
        trace!("POST {} body={}", url, body);
        let resp = self
            .http
            .post(url)
            .header("Authorization", &self.auth_header)
            .json(body)
            .send()
            .await
            .with_context(|| format!("POST {} failed", url))?;

        let status = resp.status();
        let text = resp.text().await.context("failed to read response body")?;
        trace!("  -> {} {}", status, &text);

        let val: Value = serde_json::from_str(&text)
            .with_context(|| format!("invalid JSON from ArangoDB: {}", &text))?;
        Ok(val)
    }

    /// GET and return the response body as `Value`.
    #[allow(dead_code)]
    pub async fn get(&self, url: &str) -> Result<Value> {
        trace!("GET {}", url);
        let resp = self
            .http
            .get(url)
            .header("Authorization", &self.auth_header)
            .send()
            .await
            .with_context(|| format!("GET {} failed", url))?;

        let status = resp.status();
        let text = resp.text().await.context("failed to read response body")?;
        trace!("  -> {} {}", status, &text);

        let val: Value = serde_json::from_str(&text)
            .with_context(|| format!("invalid JSON from ArangoDB: {}", &text))?;
        Ok(val)
    }

    // ------------------------------------------------------------------
    // Database operations
    // ------------------------------------------------------------------

    /// Create the target database if it does not already exist.
    pub async fn create_database_if_not_exists(&self) -> Result<()> {
        let url = self.system_url("/_api/database");
        let body = serde_json::json!({
            "name": self.database,
        });
        let resp = self.post(&url, &body).await?;

        if resp.get("error").and_then(|v| v.as_bool()).unwrap_or(false) {
            let code = resp.get("errorNum").and_then(|v| v.as_u64()).unwrap_or(0);
            // 1207 = duplicate database
            if code == 1207 {
                debug!("Database '{}' already exists", self.database);
                return Ok(());
            }
            anyhow::bail!(
                "Failed to create database '{}': {}",
                self.database,
                resp.get("errorMessage")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error")
            );
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Collection operations
    // ------------------------------------------------------------------

    /// Create a document (vertex) collection if it does not exist.
    pub async fn create_collection(&self, name: &str) -> Result<()> {
        self.create_collection_typed(name, 2).await
    }

    /// Create an edge collection if it does not exist.
    pub async fn create_edge_collection(&self, name: &str) -> Result<()> {
        self.create_collection_typed(name, 3).await
    }

    /// Internal: type 2 = document, type 3 = edge.
    async fn create_collection_typed(&self, name: &str, collection_type: u8) -> Result<()> {
        let url = self.db_url("/_api/collection");
        let body = serde_json::json!({
            "name": name,
            "type": collection_type,
        });
        let resp = self.post(&url, &body).await?;

        if resp.get("error").and_then(|v| v.as_bool()).unwrap_or(false) {
            let code = resp.get("errorNum").and_then(|v| v.as_u64()).unwrap_or(0);
            // 1207 = duplicate name
            if code == 1207 {
                debug!("Collection '{}' already exists", name);
                return Ok(());
            }
            anyhow::bail!(
                "Failed to create collection '{}': {}",
                name,
                resp.get("errorMessage")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error")
            );
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Graph operations
    // ------------------------------------------------------------------

    /// Create a named graph if it does not exist.
    pub async fn create_graph(&self, name: &str, edge_definitions: &Value) -> Result<()> {
        let url = self.db_url("/_api/gharial");
        let body = serde_json::json!({
            "name": name,
            "edgeDefinitions": edge_definitions,
        });
        let resp = self.post(&url, &body).await?;

        if resp.get("error").and_then(|v| v.as_bool()).unwrap_or(false) {
            let code = resp.get("errorNum").and_then(|v| v.as_u64()).unwrap_or(0);
            // 1925 = graph already exists
            if code == 1925 {
                debug!("Graph '{}' already exists", name);
                return Ok(());
            }
            anyhow::bail!(
                "Failed to create graph '{}': {}",
                name,
                resp.get("errorMessage")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error")
            );
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Index operations
    // ------------------------------------------------------------------

    /// Create a persistent index on a collection.
    pub async fn create_persistent_index(
        &self,
        collection: &str,
        fields: &[&str],
        unique: bool,
        sparse: bool,
    ) -> Result<()> {
        let url = self.db_url(&format!("/_api/index?collection={}", collection));
        let body = serde_json::json!({
            "type": "persistent",
            "fields": fields,
            "unique": unique,
            "sparse": sparse,
        });
        let resp = self.post(&url, &body).await?;

        if resp.get("error").and_then(|v| v.as_bool()).unwrap_or(false) {
            anyhow::bail!(
                "Failed to create index on '{}' {:?}: {}",
                collection,
                fields,
                resp.get("errorMessage")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error")
            );
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // View operations
    // ------------------------------------------------------------------

    /// Create an ArangoSearch view if it does not exist.
    pub async fn create_search_view(&self, name: &str, properties: &Value) -> Result<()> {
        let url = self.db_url("/_api/view");
        let mut body = properties.clone();
        if let Some(obj) = body.as_object_mut() {
            obj.insert("name".to_string(), Value::String(name.to_string()));
            obj.insert("type".to_string(), Value::String("arangosearch".to_string()));
        }
        let resp = self.post(&url, &body).await?;

        if resp.get("error").and_then(|v| v.as_bool()).unwrap_or(false) {
            let code = resp.get("errorNum").and_then(|v| v.as_u64()).unwrap_or(0);
            // 1207 = duplicate name
            if code == 1207 {
                debug!("View '{}' already exists", name);
                return Ok(());
            }
            anyhow::bail!(
                "Failed to create view '{}': {}",
                name,
                resp.get("errorMessage")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error")
            );
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // AQL cursor (for seeding data)
    // ------------------------------------------------------------------

    /// Execute an AQL query via the cursor API.
    pub async fn execute_aql(&self, query: &str, bind_vars: &Value) -> Result<Value> {
        let url = self.db_url("/_api/cursor");
        let body = serde_json::json!({
            "query": query,
            "bindVars": bind_vars,
        });
        let resp = self.post(&url, &body).await?;

        if resp.get("error").and_then(|v| v.as_bool()).unwrap_or(false) {
            anyhow::bail!(
                "AQL execution failed: {}",
                resp.get("errorMessage")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error")
            );
        }
        Ok(resp)
    }

    /// Return the database name.
    pub fn database(&self) -> &str {
        &self.database
    }

    /// Return the base URL.
    #[allow(dead_code)]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}
