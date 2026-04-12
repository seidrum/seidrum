//! REST API handlers.

use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use seidrum_common::events::{
    PluginDeregister, StorageDeleteRequest, StorageDeleteResponse, StorageGetRequest,
    StorageGetResponse, StorageListRequest, StorageListResponse, StorageSetRequest,
    StorageSetResponse, ToolCallRequest, ToolCallResponse,
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::AppState;

// ---------------------------------------------------------------------------
// Query parameters
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CapabilitySearchParams {
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_limit() -> u32 {
    10
}

// ---------------------------------------------------------------------------
// Request/response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CapabilityCallBody {
    pub arguments: serde_json::Value,
}

#[derive(Deserialize)]
pub struct StorageBody {
    pub plugin_id: String,
    #[serde(default = "default_namespace")]
    pub namespace: String,
    pub key: Option<String>,
    pub value: Option<serde_json::Value>,
}

fn default_namespace() -> String {
    "default".to_string()
}

#[derive(Serialize)]
pub struct PluginListResponse {
    plugins: Vec<PluginSummary>,
}

#[derive(Serialize)]
pub struct PluginSummary {
    id: String,
}

#[derive(Serialize)]
pub struct HealthResponse {
    status: String,
    connected_plugins: usize,
    uptime_seconds: u64,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /api/v1/health`
pub async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "healthy".into(),
        connected_plugins: state.connections.connected_count(),
        uptime_seconds: state.start_time.elapsed().as_secs(),
    })
}

/// `GET /api/v1/plugins`
pub async fn list_plugins(State(state): State<AppState>) -> Json<PluginListResponse> {
    let ids = state.connections.list_plugin_ids();
    Json(PluginListResponse {
        plugins: ids.into_iter().map(|id| PluginSummary { id }).collect(),
    })
}

/// `DELETE /api/v1/plugins/:id`
pub async fn deregister_plugin(
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
) -> StatusCode {
    state.connections.deregister(&plugin_id).await;

    let dereg = PluginDeregister {
        id: plugin_id.clone(),
    };
    if let Ok(bytes) = serde_json::to_vec(&dereg) {
        let _ = state.nats.publish_bytes("plugin.deregister", bytes).await;
    }

    StatusCode::NO_CONTENT
}

/// `POST /api/v1/capabilities/:id/call`
pub async fn call_capability(
    State(state): State<AppState>,
    Path(capability_id): Path<String>,
    Json(body): Json<CapabilityCallBody>,
) -> impl IntoResponse {
    let call_subject = format!("capability.call.{}", capability_id);

    let req = ToolCallRequest {
        tool_id: capability_id.clone(),
        plugin_id: "api-gateway".into(),
        arguments: body.arguments,
        correlation_id: None,
    };

    match tokio::time::timeout(
        Duration::from_secs(30),
        state
            .nats
            .request::<_, ToolCallResponse>(&call_subject, &req),
    )
    .await
    {
        Ok(Ok(resp)) => {
            let status = if resp.is_error {
                StatusCode::UNPROCESSABLE_ENTITY
            } else {
                StatusCode::OK
            };
            (
                status,
                Json(
                    serde_json::to_value(resp)
                        .unwrap_or_else(|_| serde_json::json!({"error": "serialization failed"})),
                ),
            )
                .into_response()
        }
        Ok(Err(err)) => {
            warn!(%err, %capability_id, "Capability call failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": err.to_string()})),
            )
                .into_response()
        }
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({"error": "Capability call timed out"})),
        )
            .into_response(),
    }
}

/// `GET /api/v1/capabilities`
pub async fn search_capabilities(
    State(state): State<AppState>,
    Query(params): Query<CapabilitySearchParams>,
) -> impl IntoResponse {
    use seidrum_common::events::{ToolSearchRequest, ToolSearchResponse};

    let req = ToolSearchRequest {
        query_text: params.query,
        limit: Some(params.limit),
        kind_filter: params.kind,
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, ToolSearchResponse>("capability.search", &req),
    )
    .await
    {
        Ok(Ok(resp)) => (
            StatusCode::OK,
            Json(
                serde_json::to_value(resp)
                    .unwrap_or_else(|_| serde_json::json!({"error": "serialization failed"})),
            ),
        )
            .into_response(),
        Ok(Err(err)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({"error": "Search timed out"})),
        )
            .into_response(),
    }
}

/// `POST /api/v1/storage/get`
pub async fn storage_get(
    State(state): State<AppState>,
    Json(body): Json<StorageBody>,
) -> impl IntoResponse {
    let key = match body.key {
        Some(k) => k,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "key is required"})),
            )
                .into_response()
        }
    };

    let req = StorageGetRequest {
        plugin_id: body.plugin_id,
        namespace: body.namespace,
        key,
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, StorageGetResponse>("storage.get", &req),
    )
    .await
    {
        Ok(Ok(resp)) => Json(
            serde_json::to_value(resp)
                .unwrap_or_else(|_| serde_json::json!({"error": "serialization failed"})),
        )
        .into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({"error": "timeout"})),
        )
            .into_response(),
    }
}

/// `POST /api/v1/storage/set`
pub async fn storage_set(
    State(state): State<AppState>,
    Json(body): Json<StorageBody>,
) -> impl IntoResponse {
    let key = match body.key {
        Some(k) => k,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "key is required"})),
            )
                .into_response()
        }
    };
    let value = body.value.unwrap_or(serde_json::json!(null));

    let req = StorageSetRequest {
        plugin_id: body.plugin_id,
        namespace: body.namespace,
        key,
        value,
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, StorageSetResponse>("storage.set", &req),
    )
    .await
    {
        Ok(Ok(resp)) => Json(
            serde_json::to_value(resp)
                .unwrap_or_else(|_| serde_json::json!({"error": "serialization failed"})),
        )
        .into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({"error": "timeout"})),
        )
            .into_response(),
    }
}

/// `POST /api/v1/storage/delete`
pub async fn storage_delete(
    State(state): State<AppState>,
    Json(body): Json<StorageBody>,
) -> impl IntoResponse {
    let key = match body.key {
        Some(k) => k,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "key is required"})),
            )
                .into_response()
        }
    };

    let req = StorageDeleteRequest {
        plugin_id: body.plugin_id,
        namespace: body.namespace,
        key,
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, StorageDeleteResponse>("storage.delete", &req),
    )
    .await
    {
        Ok(Ok(resp)) => Json(
            serde_json::to_value(resp)
                .unwrap_or_else(|_| serde_json::json!({"error": "serialization failed"})),
        )
        .into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({"error": "timeout"})),
        )
            .into_response(),
    }
}

/// `POST /api/v1/storage/list`
pub async fn storage_list(
    State(state): State<AppState>,
    Json(body): Json<StorageBody>,
) -> impl IntoResponse {
    let req = StorageListRequest {
        plugin_id: body.plugin_id,
        namespace: body.namespace,
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, StorageListResponse>("storage.list", &req),
    )
    .await
    {
        Ok(Ok(resp)) => Json(
            serde_json::to_value(resp)
                .unwrap_or_else(|_| serde_json::json!({"error": "serialization failed"})),
        )
        .into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({"error": "timeout"})),
        )
            .into_response(),
    }
}
