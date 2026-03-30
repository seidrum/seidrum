//! Dashboard backend handlers.
//!
//! These endpoints power the admin web dashboard and are mounted
//! under `/api/v1/`.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use chrono::Utc;
use seidrum_common::events::{
    ConfigUpdated, ConversationGetRequest, ConversationListRequest, ConversationListResponse,
    PluginHealthRequest, PluginHealthResponse, PluginRegister, SkillGetRequest, SkillListRequest,
    SkillListResponse, StorageGetRequest, StorageGetResponse, StorageSetRequest,
    StorageSetResponse,
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::AppState;

// ---------------------------------------------------------------------------
// Registry query types (mirror of kernel's RegistryQuery)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(tag = "query_type")]
#[allow(dead_code)]
enum RegistryQuery {
    #[serde(rename = "list_plugins")]
    ListPlugins,
    #[serde(rename = "get_plugin")]
    GetPlugin { plugin_id: String },
    #[serde(rename = "get_config_schema")]
    GetConfigSchema { plugin_id: String },
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct RegistryQueryResponse {
    success: bool,
    plugin: Option<PluginRegister>,
    plugins: Option<Vec<PluginRegister>>,
    config_schema: Option<serde_json::Value>,
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct DashboardOverview {
    pub uptime_seconds: u64,
    pub plugins: Vec<PluginSummary>,
}

#[derive(Serialize)]
pub struct PluginSummary {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub health_status: String,
    pub has_config_schema: bool,
}

#[derive(Serialize)]
pub struct PluginConfigResponse {
    pub plugin_id: String,
    pub config: Option<serde_json::Value>,
    pub schema: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub struct ConfigUpdateBody {
    pub config: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /api/v1/dashboard/overview`
///
/// Returns system overview with all plugins and their health status.
pub async fn overview(State(state): State<AppState>) -> impl IntoResponse {
    // Get all registered plugins from the kernel registry
    let plugins_result: Result<RegistryQueryResponse, _> = tokio::time::timeout(
        Duration::from_secs(3),
        state
            .nats
            .request::<_, RegistryQueryResponse>("registry.query", &RegistryQuery::ListPlugins),
    )
    .await
    .map_err(|_| "timeout")
    .and_then(|r| r.map_err(|_| "nats error"));

    let registered_plugins = plugins_result
        .ok()
        .and_then(|r| r.plugins)
        .unwrap_or_default();

    // Health-check all plugins in parallel with short timeout
    let health_futures: Vec<_> = registered_plugins
        .iter()
        .map(|p| {
            let nats = state.nats.clone();
            let health_subject = p.health_subject.clone();
            let plugin_id = p.id.clone();
            async move {
                let req = PluginHealthRequest {};
                let result = tokio::time::timeout(
                    Duration::from_secs(2),
                    nats.request::<_, PluginHealthResponse>(&health_subject, &req),
                )
                .await;
                match result {
                    Ok(Ok(resp)) => (plugin_id, resp.status),
                    _ => (plugin_id, "unreachable".to_string()),
                }
            }
        })
        .collect();

    let health_results = futures::future::join_all(health_futures).await;
    let health_map: std::collections::HashMap<String, String> =
        health_results.into_iter().collect();

    let plugins: Vec<PluginSummary> = registered_plugins
        .iter()
        .map(|p| PluginSummary {
            id: p.id.clone(),
            name: p.name.clone(),
            version: p.version.clone(),
            description: p.description.clone(),
            health_status: health_map
                .get(&p.id)
                .cloned()
                .unwrap_or_else(|| "unknown".to_string()),
            has_config_schema: p.config_schema.is_some(),
        })
        .collect();

    Json(DashboardOverview {
        uptime_seconds: state.start_time.elapsed().as_secs(),
        plugins,
    })
}

/// `GET /api/v1/dashboard/plugins/:id/health`
pub async fn plugin_health(
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
) -> impl IntoResponse {
    let health_subject = format!("plugin.{}.health", plugin_id);
    let req = PluginHealthRequest {};

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, PluginHealthResponse>(&health_subject, &req),
    )
    .await
    {
        Ok(Ok(resp)) => {
            Json(serde_json::to_value(&resp).unwrap_or(serde_json::json!(null))).into_response()
        }
        Ok(Err(err)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({"error": "Health check timed out"})),
        )
            .into_response(),
    }
}

/// `GET /api/v1/dashboard/plugins/:id/config`
///
/// Returns current config + schema for a plugin.
pub async fn get_plugin_config(
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
) -> impl IntoResponse {
    // Get schema from registry
    let schema = get_schema(&state, &plugin_id).await;

    // Get current config from storage
    let config = get_stored_config(&state, &plugin_id).await;

    Json(PluginConfigResponse {
        plugin_id,
        config,
        schema,
    })
}

/// `GET /api/v1/dashboard/plugins/:id/config/schema`
pub async fn get_config_schema(
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
) -> impl IntoResponse {
    match get_schema(&state, &plugin_id).await {
        Some(schema) => Json(schema).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "No config schema for this plugin"})),
        )
            .into_response(),
    }
}

/// `PUT /api/v1/dashboard/plugins/:id/config`
///
/// Validates config against schema, persists, and notifies the plugin.
pub async fn update_plugin_config(
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
    Json(body): Json<ConfigUpdateBody>,
) -> impl IntoResponse {
    // 1. Get schema
    let schema = match get_schema(&state, &plugin_id).await {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Plugin does not have a config schema"})),
            )
                .into_response()
        }
    };

    // 2. Validate against schema
    match jsonschema::validator_for(&schema) {
        Ok(validator) => {
            let error_messages: Vec<String> = validator
                .iter_errors(&body.config)
                .map(|e| e.to_string())
                .collect();

            if !error_messages.is_empty() {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(serde_json::json!({
                        "error": "Config validation failed",
                        "details": error_messages
                    })),
                )
                    .into_response();
            }
        }
        Err(err) => {
            warn!(%err, %plugin_id, "Invalid config schema");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Invalid schema: {}", err)})),
            )
                .into_response();
        }
    }

    // 3. Persist to storage
    let store_req = StorageSetRequest {
        plugin_id: plugin_id.clone(),
        namespace: "config".to_string(),
        key: "current".to_string(),
        value: body.config.clone(),
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, StorageSetResponse>("storage.set", &store_req),
    )
    .await
    {
        Ok(Ok(resp)) if resp.success => {}
        Ok(Ok(resp)) => {
            let err = resp.error.unwrap_or_else(|| "unknown".to_string());
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Storage error: {}", err)})),
            )
                .into_response();
        }
        Ok(Err(err)) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("Storage error: {}", err)})),
            )
                .into_response()
        }
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                Json(serde_json::json!({"error": "Storage request timed out"})),
            )
                .into_response()
        }
    }

    // 4. Notify the plugin
    let update = ConfigUpdated {
        plugin_id: plugin_id.clone(),
        config: body.config,
        updated_by: "dashboard".to_string(),
        timestamp: Utc::now(),
    };

    let subject = format!("plugin.{}.config.update", plugin_id);
    if let Ok(bytes) = serde_json::to_vec(&update) {
        let _ = state.nats.inner().publish(subject, bytes.into()).await;
    }

    Json(serde_json::json!({"success": true})).into_response()
}

// ---------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------

/// `GET /api/v1/dashboard/skills`
///
/// Returns all skills from the brain.
pub async fn list_skills(State(state): State<AppState>) -> impl IntoResponse {
    let req = SkillListRequest {
        source_filter: None,
        limit: Some(100),
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, SkillListResponse>("brain.skill.list", &req),
    )
    .await
    {
        Ok(Ok(resp)) => {
            Json(serde_json::to_value(&resp).unwrap_or(serde_json::json!(null))).into_response()
        }
        Ok(Err(err)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({"error": "Skill list request timed out"})),
        )
            .into_response(),
    }
}

/// `GET /api/v1/dashboard/skills/:id`
///
/// Returns a single skill by ID.
pub async fn get_skill(
    State(state): State<AppState>,
    Path(skill_id): Path<String>,
) -> impl IntoResponse {
    let req = SkillGetRequest { skill_id };

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, serde_json::Value>("brain.skill.get", &req),
    )
    .await
    {
        Ok(Ok(resp)) => {
            if resp.is_null() {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "Skill not found"})),
                )
                    .into_response()
            } else {
                Json(resp).into_response()
            }
        }
        Ok(Err(err)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({"error": "Skill get request timed out"})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Conversations (dashboard)
// ---------------------------------------------------------------------------

/// `GET /api/v1/dashboard/conversations`
///
/// Returns recent conversations from the brain.
pub async fn list_conversations_dashboard(State(state): State<AppState>) -> impl IntoResponse {
    let req = ConversationListRequest {
        agent_id: "".to_string(),
        platform: None,
        limit: 50,
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, ConversationListResponse>("brain.conversation.list", &req),
    )
    .await
    {
        Ok(Ok(resp)) => {
            Json(serde_json::to_value(&resp).unwrap_or(serde_json::json!(null))).into_response()
        }
        Ok(Err(err)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({"error": "Conversation list request timed out"})),
        )
            .into_response(),
    }
}

/// `GET /api/v1/dashboard/conversations/:id`
///
/// Returns a single conversation with messages.
pub async fn get_conversation_dashboard(
    State(state): State<AppState>,
    Path(conversation_id): Path<String>,
) -> impl IntoResponse {
    let req = ConversationGetRequest {
        conversation_id,
        max_messages: 100,
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, serde_json::Value>("brain.conversation.get", &req),
    )
    .await
    {
        Ok(Ok(resp)) => {
            if resp.is_null() {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "Conversation not found"})),
                )
                    .into_response()
            } else {
                Json(resp).into_response()
            }
        }
        Ok(Err(err)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({"error": "Conversation get request timed out"})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn get_schema(state: &AppState, plugin_id: &str) -> Option<serde_json::Value> {
    let query = RegistryQuery::GetConfigSchema {
        plugin_id: plugin_id.to_string(),
    };
    match tokio::time::timeout(
        Duration::from_secs(3),
        state
            .nats
            .request::<_, RegistryQueryResponse>("registry.query", &query),
    )
    .await
    {
        Ok(Ok(resp)) => resp.config_schema,
        _ => None,
    }
}

async fn get_stored_config(state: &AppState, plugin_id: &str) -> Option<serde_json::Value> {
    let req = StorageGetRequest {
        plugin_id: plugin_id.to_string(),
        namespace: "config".to_string(),
        key: "current".to_string(),
    };
    match tokio::time::timeout(
        Duration::from_secs(3),
        state
            .nats
            .request::<_, StorageGetResponse>("storage.get", &req),
    )
    .await
    {
        Ok(Ok(resp)) if resp.found => resp.value,
        _ => None,
    }
}
