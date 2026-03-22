//! Brain initialization: creates collections, graph, indexes, views, and seed data.

use anyhow::{Context, Result};
use tracing::info;

use super::client::ArangoClient;

/// Vertex collections defined in BRAIN_SCHEMA.md.
const VERTEX_COLLECTIONS: &[&str] = &[
    "entities",
    "content",
    "facts",
    "scopes",
    "tasks",
    "event_types",
    "capabilities",
    "plugin_storage",
];

/// Edge collections defined in BRAIN_SCHEMA.md.
const EDGE_COLLECTIONS: &[&str] = &[
    "relates_to",
    "mentions",
    "scoped_to",
    "derived_from",
    "supersedes",
    "participated_in",
    "task_relates",
];

/// Run full brain initialization.
///
/// This is idempotent — it uses "create if not exists" semantics for every
/// resource so it can safely be run multiple times.
pub async fn initialize_brain(client: &ArangoClient) -> Result<()> {
    info!("Creating database '{}' if needed...", client.database());

    // 1. Create database
    client
        .create_database_if_not_exists()
        .await
        .context("database creation")?;
    info!("Database '{}' ready", client.database());

    // 2. Create vertex collections
    info!("Creating vertex collections...");
    for name in VERTEX_COLLECTIONS {
        client
            .create_collection(name)
            .await
            .with_context(|| format!("creating vertex collection '{}'", name))?;
        info!("  [OK] {}", name);
    }

    // 3. Create edge collections
    info!("Creating edge collections...");
    for name in EDGE_COLLECTIONS {
        client
            .create_edge_collection(name)
            .await
            .with_context(|| format!("creating edge collection '{}'", name))?;
        info!("  [OK] {}", name);
    }

    // 4. Create named graph "brain"
    info!("Creating named graph 'brain'...");
    create_brain_graph(client).await.context("creating brain graph")?;
    info!("  [OK] brain graph");

    // 5. Create indexes
    info!("Creating indexes...");
    create_indexes(client).await.context("creating indexes")?;
    info!("  [OK] indexes");

    // 6. Create ArangoSearch view
    info!("Creating ArangoSearch view 'content_search'...");
    create_search_view(client)
        .await
        .context("creating search view")?;
    info!("  [OK] content_search view");

    // 7. Seed root scope
    info!("Seeding root scope...");
    seed_root_scope(client).await.context("seeding root scope")?;
    info!("  [OK] root scope");

    info!("Brain initialization complete.");
    Ok(())
}

/// Create the named graph "brain" with edge definitions from BRAIN_SCHEMA.md.
async fn create_brain_graph(client: &ArangoClient) -> Result<()> {
    let edge_definitions = serde_json::json!([
        {
            "collection": "relates_to",
            "from": ["entities"],
            "to": ["entities"]
        },
        {
            "collection": "mentions",
            "from": ["content"],
            "to": ["entities"]
        },
        {
            "collection": "scoped_to",
            "from": ["entities", "content", "facts", "tasks"],
            "to": ["scopes"]
        },
        {
            "collection": "derived_from",
            "from": ["facts", "content"],
            "to": ["content"]
        },
        {
            "collection": "supersedes",
            "from": ["facts"],
            "to": ["facts"]
        },
        {
            "collection": "participated_in",
            "from": ["entities"],
            "to": ["content"]
        },
        {
            "collection": "task_relates",
            "from": ["tasks"],
            "to": ["entities", "content", "tasks"]
        }
    ]);

    client.create_graph("brain", &edge_definitions).await
}

/// Create persistent indexes for common query fields.
async fn create_indexes(client: &ArangoClient) -> Result<()> {
    // -- entities --
    client
        .create_persistent_index("entities", &["type"], false, false)
        .await
        .context("entities.type index")?;
    client
        .create_persistent_index("entities", &["name"], false, false)
        .await
        .context("entities.name index")?;

    // -- content --
    client
        .create_persistent_index("content", &["type"], false, false)
        .await
        .context("content.type index")?;
    client
        .create_persistent_index("content", &["channel"], false, false)
        .await
        .context("content.channel index")?;
    client
        .create_persistent_index("content", &["timestamp"], false, false)
        .await
        .context("content.timestamp index")?;

    // -- facts --
    client
        .create_persistent_index("facts", &["subject"], false, false)
        .await
        .context("facts.subject index")?;
    client
        .create_persistent_index("facts", &["predicate"], false, false)
        .await
        .context("facts.predicate index")?;
    client
        .create_persistent_index("facts", &["valid_from", "valid_to"], false, true)
        .await
        .context("facts.valid_from+valid_to index")?;
    client
        .create_persistent_index("facts", &["superseded_by"], false, true)
        .await
        .context("facts.superseded_by index")?;
    client
        .create_persistent_index("facts", &["confidence"], false, false)
        .await
        .context("facts.confidence index")?;
    client
        .create_persistent_index("facts", &["last_reinforced"], false, true)
        .await
        .context("facts.last_reinforced index")?;

    // -- scopes --
    client
        .create_persistent_index("scopes", &["type"], false, false)
        .await
        .context("scopes.type index")?;
    client
        .create_persistent_index("scopes", &["parent_scope"], false, true)
        .await
        .context("scopes.parent_scope index")?;
    client
        .create_persistent_index("scopes", &["status"], false, false)
        .await
        .context("scopes.status index")?;

    // -- tasks --
    client
        .create_persistent_index("tasks", &["status"], false, false)
        .await
        .context("tasks.status index")?;
    client
        .create_persistent_index("tasks", &["assigned_agent"], false, false)
        .await
        .context("tasks.assigned_agent index")?;
    client
        .create_persistent_index("tasks", &["priority"], false, false)
        .await
        .context("tasks.priority index")?;

    // -- event_types --
    client
        .create_persistent_index("event_types", &["subject"], true, false)
        .await
        .context("event_types.subject index")?;

    // -- edge indexes --
    // scoped_to edges are queried by scope target frequently
    client
        .create_persistent_index("scoped_to", &["relevance"], false, false)
        .await
        .context("scoped_to.relevance index")?;

    // relates_to edges queried by type
    client
        .create_persistent_index("relates_to", &["type"], false, false)
        .await
        .context("relates_to.type index")?;

    // mentions queried by mention_type
    client
        .create_persistent_index("mentions", &["mention_type"], false, false)
        .await
        .context("mentions.mention_type index")?;

    // -- tools --
    client
        .create_persistent_index("capabilities", &["tool_id"], true, false)
        .await
        .context("tools.tool_id index")?;
    client
        .create_persistent_index("capabilities", &["plugin_id"], false, false)
        .await
        .context("tools.plugin_id index")?;
    client
        .create_persistent_index("capabilities", &["name"], false, false)
        .await
        .context("tools.name index")?;

    // TODO: Vector indexes for entities.embedding and content.embedding.
    // ArangoDB 3.12+ supports vector indexes natively, but they require
    // specific server configuration. Add these once the ArangoDB instance
    // is confirmed to support the vector index type:
    //
    //   db.entities.ensureIndex({
    //     type: "vector", fields: ["embedding"],
    //     params: { dimension: 1536, metric: "cosine", nLists: 100 }
    //   });
    //
    //   db.content.ensureIndex({
    //     type: "vector", fields: ["embedding"],
    //     params: { dimension: 1536, metric: "cosine", nLists: 200 }
    //   });

    Ok(())
}

/// Create the ArangoSearch view "content_search" for full-text search
/// over content.raw_text, content.summary, and entities.name.
async fn create_search_view(client: &ArangoClient) -> Result<()> {
    let view_definition = serde_json::json!({
        "links": {
            "content": {
                "fields": {
                    "raw_text": {
                        "analyzers": ["text_en"]
                    },
                    "summary": {
                        "analyzers": ["text_en"]
                    }
                }
            },
            "entities": {
                "fields": {
                    "name": {
                        "analyzers": ["text_en", "identity"]
                    }
                }
            },
            "capabilities": {
                "fields": {
                    "summary_md": {
                        "analyzers": ["text_en"]
                    },
                    "manual_md": {
                        "analyzers": ["text_en"]
                    }
                }
            }
        }
    });

    client.create_search_view("content_search", &view_definition).await
}

/// Seed the root scope document if it does not already exist.
async fn seed_root_scope(client: &ArangoClient) -> Result<()> {
    let query = r#"
        UPSERT { _key: "scope_root" }
        INSERT {
            _key: "scope_root",
            name: "Root",
            type: "identity",
            description: "Root identity scope — always accessible",
            parent_scope: null,
            access_rules: {
                cross_scope_read: [],
                cross_scope_write: []
            },
            status: "active",
            created_at: DATE_ISO8601(DATE_NOW())
        }
        UPDATE {}
        IN scopes
        RETURN { new: NEW, type: IS_NULL(OLD) ? "inserted" : "existing" }
    "#;

    let result = client
        .execute_aql(query, &serde_json::json!({}))
        .await?;

    if let Some(results) = result.get("result").and_then(|v| v.as_array()) {
        if let Some(first) = results.first() {
            let op_type = first
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            if op_type == "inserted" {
                info!("  Root scope created (scope_root)");
            } else {
                info!("  Root scope already exists (scope_root)");
            }
        }
    }

    Ok(())
}
