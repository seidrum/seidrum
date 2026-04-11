use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tracing::{error, info};

use crate::management::state::ManagementState;

// ============================================================================
// Local types (mirrors seidrum-daemon config.rs)
// ============================================================================

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PluginsConfig {
    pub plugins: BTreeMap<String, PluginEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PluginEntry {
    pub binary: String,
    pub enabled: bool,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

// ============================================================================
// Response types
// ============================================================================

#[derive(Debug, Serialize, Clone)]
pub struct PluginListItem {
    pub name: String,
    pub binary: String,
    pub enabled: bool,
    pub running: bool,
    pub env_vars: usize,
}

#[derive(Debug, Serialize, Clone)]
pub struct PluginDetailResponse {
    pub name: String,
    pub binary: String,
    pub enabled: bool,
    pub running: bool,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct PluginConfigSchemaItem {
    pub key: String,
    pub has_value: bool,
}

// ============================================================================
// Handlers
// ============================================================================

/// Load plugins.yaml, query NATS registry for running state, merge views.
pub async fn list_plugins(
    State(state): State<ManagementState>,
) -> Result<Json<Vec<PluginListItem>>, (StatusCode, String)> {
    let config = load_plugins_config(&state.plugins_yaml())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let running_plugins = query_running_plugins(&state).await.unwrap_or_default();

    let items: Vec<PluginListItem> = config
        .plugins
        .iter()
        .map(|(name, entry)| {
            let running = running_plugins.contains(name);
            PluginListItem {
                name: name.clone(),
                binary: entry.binary.clone(),
                enabled: entry.enabled,
                running,
                env_vars: entry.env.len(),
            }
        })
        .collect();

    Ok(Json(items))
}

/// Enable a plugin by name.
pub async fn enable_plugin(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> Result<Json<PluginDetailResponse>, (StatusCode, String)> {
    set_plugin_enabled(&state, &name, true)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    info!("Plugin '{}' enabled", name);

    get_plugin_detail(&state, &name)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// Disable a plugin by name.
pub async fn disable_plugin(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> Result<Json<PluginDetailResponse>, (StatusCode, String)> {
    set_plugin_enabled(&state, &name, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    info!("Plugin '{}' disabled", name);

    get_plugin_detail(&state, &name)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// Start a plugin by publishing to daemon.plugin.start NATS subject.
/// Returns 202 Accepted (async operation).
pub async fn start_plugin(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let payload = serde_json::json!({ "plugin": name.clone() }).to_string();

    state
        .nats
        .publish_bytes("daemon.plugin.start".to_string(), payload)
        .await
        .map_err(|e| {
            error!("Failed to publish plugin.start: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;

    info!("Plugin '{}' start signal sent", name);
    Ok(StatusCode::ACCEPTED)
}

/// Stop a plugin by publishing to daemon.plugin.stop NATS subject.
/// Returns 202 Accepted (async operation).
pub async fn stop_plugin(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let payload = serde_json::json!({ "plugin": name.clone() }).to_string();

    state
        .nats
        .publish_bytes("daemon.plugin.stop".to_string(), payload)
        .await
        .map_err(|e| {
            error!("Failed to publish plugin.stop: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;

    info!("Plugin '{}' stop signal sent", name);
    Ok(StatusCode::ACCEPTED)
}

/// Get plugin config schema: return env var names from plugins.yaml.
pub async fn plugin_config_schema(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> Result<Json<Vec<PluginConfigSchemaItem>>, (StatusCode, String)> {
    let config = load_plugins_config(&state.plugins_yaml())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let entry = config.plugins.get(&name).ok_or((
        StatusCode::NOT_FOUND,
        format!("Plugin '{}' not found", name),
    ))?;

    let schema: Vec<PluginConfigSchemaItem> = entry
        .env
        .iter()
        .map(|(key, value)| PluginConfigSchemaItem {
            key: key.clone(),
            has_value: !value.is_empty(),
        })
        .collect();

    Ok(Json(schema))
}

// ============================================================================
// Internal helpers
// ============================================================================

pub fn load_plugins_config(path: &std::path::Path) -> anyhow::Result<PluginsConfig> {
    let contents = std::fs::read_to_string(path)?;
    let config: PluginsConfig = serde_yaml::from_str(&contents)?;
    Ok(config)
}

pub fn save_plugins_config(path: &std::path::Path, config: &PluginsConfig) -> anyhow::Result<()> {
    let contents = serde_yaml::to_string(config)?;
    std::fs::write(path, contents)?;
    Ok(())
}

async fn set_plugin_enabled(
    state: &ManagementState,
    name: &str,
    enabled: bool,
) -> anyhow::Result<()> {
    let yaml_path = state.plugins_yaml();
    let mut config = load_plugins_config(&yaml_path)?;

    config
        .plugins
        .get_mut(name)
        .ok_or_else(|| anyhow::anyhow!("Plugin '{}' not found", name))?
        .enabled = enabled;

    save_plugins_config(&yaml_path, &config)?;
    Ok(())
}

async fn get_plugin_detail(
    state: &ManagementState,
    name: &str,
) -> anyhow::Result<Json<PluginDetailResponse>> {
    let config = load_plugins_config(&state.plugins_yaml())?;

    let entry = config
        .plugins
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("Plugin '{}' not found", name))?;

    let running_plugins = query_running_plugins(state).await.unwrap_or_default();
    let running = running_plugins.contains(name);

    Ok(Json(PluginDetailResponse {
        name: name.to_string(),
        binary: entry.binary.clone(),
        enabled: entry.enabled,
        running,
        env: entry.env.clone(),
    }))
}

/// Query NATS registry.query subject to get list of running plugins.
/// This subscribes to registry.query and expects a response with running plugin names.
async fn query_running_plugins(
    state: &ManagementState,
) -> anyhow::Result<std::collections::HashSet<String>> {
    // Try to request plugin list from registry via NATS
    // The registry.query subject expects a response with plugin names
    let request = serde_json::json!({ "type": "list_plugins" }).to_string();

    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state.nats.request_bytes("registry.query", request),
    )
    .await
    {
        Ok(Ok(reply)) => {
            let response: std::collections::HashSet<String> =
                serde_json::from_slice(&reply).unwrap_or_default();
            Ok(response)
        }
        Err(_) => {
            tracing::warn!("Failed to query plugin registry: timeout");
            Ok(std::collections::HashSet::new())
        }
        Ok(Err(e)) => {
            tracing::warn!("Failed to query plugin registry: {}", e);
            Ok(std::collections::HashSet::new())
        }
    }
}
