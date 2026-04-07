use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};
use seidrum_common::config::AgentDefinition;
use std::collections::BTreeMap;
use std::path::PathBuf;
use tracing::{error, info};

use crate::management::state::ManagementState;

// ============================================================================
// Preset types
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetFile {
    pub preset: Preset,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub icon: String,
    pub plugins: PresetPlugins,
    pub agents: PresetAgents,
    #[serde(default)]
    pub env_required: Vec<EnvRequirement>,
    #[serde(default)]
    pub llm_provider: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetPlugins {
    pub required: Vec<String>,
    #[serde(default)]
    pub recommended: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetAgents {
    pub required: Vec<String>,
    #[serde(default)]
    pub recommended: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvRequirement {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub help: String,
    #[serde(default)]
    pub auto_generate: Option<String>,
}

// ============================================================================
// Response types
// ============================================================================

#[derive(Debug, Serialize)]
pub struct PresetListItem {
    pub id: String,
    pub name: String,
    pub description: String,
    pub icon: String,
    pub required_plugins: usize,
    pub recommended_plugins: usize,
    pub required_agents: usize,
}

#[derive(Debug, Serialize)]
pub struct ApplyPresetResponse {
    pub enabled_plugins: Vec<String>,
    pub enabled_agents: Vec<String>,
    pub missing_env: Vec<EnvRequirement>,
}

#[derive(Debug, Deserialize)]
pub struct ApplyPresetRequest {
    #[serde(default)]
    pub include_recommended: bool,
}

// ============================================================================
// Handlers
// ============================================================================

/// List all presets from config/presets/ directory.
pub async fn list_presets(
    State(state): State<ManagementState>,
) -> Result<Json<Vec<PresetListItem>>, (StatusCode, String)> {
    let presets_dir = &state.presets_dir;

    // Create directory if it doesn't exist
    if !presets_dir.exists() {
        std::fs::create_dir_all(presets_dir)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    let entries = std::fs::read_dir(presets_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut presets = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                error!("Error reading presets directory entry: {}", e);
                continue;
            }
        };

        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        if let Some(ext) = path.extension() {
            if ext != "yaml" && ext != "yml" {
                continue;
            }
        } else {
            continue;
        }

        match load_preset(&path) {
            Ok(preset_file) => {
                let preset = &preset_file.preset;
                presets.push(PresetListItem {
                    id: preset.id.clone(),
                    name: preset.name.clone(),
                    description: preset.description.clone(),
                    icon: preset.icon.clone(),
                    required_plugins: preset.plugins.required.len(),
                    recommended_plugins: preset.plugins.recommended.len(),
                    required_agents: preset.agents.required.len(),
                });
            }
            Err(e) => {
                error!("Error loading preset from {:?}: {}", path, e);
            }
        }
    }

    presets.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Json(presets))
}

/// Apply a preset by id.
pub async fn apply_preset(
    State(state): State<ManagementState>,
    Path(id): Path<String>,
    Json(req): Json<ApplyPresetRequest>,
) -> Result<Json<ApplyPresetResponse>, (StatusCode, String)> {
    let preset_path = find_preset_file(&state.presets_dir, &id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("Preset '{}' not found", id),
        ))?;

    let preset_file = load_preset(&preset_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let preset = &preset_file.preset;

    let mut enabled_plugins = Vec::new();
    let mut enabled_agents = Vec::new();
    let mut missing_env = Vec::new();

    // Load current plugins config
    let plugins_yaml = state.plugins_yaml();
    let mut plugins_config = match super::plugins::load_plugins_config(&plugins_yaml) {
        Ok(cfg) => cfg,
        Err(_) => super::plugins::PluginsConfig {
            plugins: BTreeMap::new(),
        },
    };

    // Enable required plugins
    for plugin_name in &preset.plugins.required {
        if let Some(entry) = plugins_config.plugins.get_mut(plugin_name) {
            entry.enabled = true;
            enabled_plugins.push(plugin_name.clone());
        }
    }

    // Enable recommended plugins if requested
    if req.include_recommended {
        for plugin_name in &preset.plugins.recommended {
            if let Some(entry) = plugins_config.plugins.get_mut(plugin_name) {
                entry.enabled = true;
                enabled_plugins.push(plugin_name.clone());
            }
        }
    }

    // Save updated plugins config
    super::plugins::save_plugins_config(&plugins_yaml, &plugins_config)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Enable required agents
    for agent_id in &preset.agents.required {
        match set_agent_enabled(&state, agent_id, true).await {
            Ok(_) => {
                enabled_agents.push(agent_id.clone());
            }
            Err(e) => {
                error!("Failed to enable agent '{}': {}", agent_id, e);
            }
        }
    }

    // Enable recommended agents if requested
    if req.include_recommended {
        for agent_id in &preset.agents.recommended {
            match set_agent_enabled(&state, agent_id, true).await {
                Ok(_) => {
                    enabled_agents.push(agent_id.clone());
                }
                Err(e) => {
                    error!("Failed to enable agent '{}': {}", agent_id, e);
                }
            }
        }
    }

    // Enable LLM provider plugin if specified
    if let Some(llm_provider) = &preset.llm_provider {
        if let Some(entry) = plugins_config.plugins.get_mut(llm_provider) {
            entry.enabled = true;
            enabled_plugins.push(llm_provider.clone());
        }
        super::plugins::save_plugins_config(&plugins_yaml, &plugins_config)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    // Check for missing env vars
    let env_file = &state.env_file;
    let env_content = std::fs::read_to_string(env_file).unwrap_or_default();
    for env_req in &preset.env_required {
        if !env_content.contains(&format!("{}=", env_req.key)) {
            missing_env.push(env_req.clone());
        }
    }

    // Publish reload signals
    let _ = state
        .nats
        .publish("kernel.agents.reload".to_string(), "".into())
        .await;

    for plugin_name in &enabled_plugins {
        let payload = serde_json::json!({ "plugin": plugin_name }).to_string();
        let _ = state
            .nats
            .publish("daemon.plugin.start".to_string(), payload.into())
            .await;
    }

    info!(
        "Preset '{}' applied: {} plugins, {} agents enabled",
        id,
        enabled_plugins.len(),
        enabled_agents.len()
    );

    Ok(Json(ApplyPresetResponse {
        enabled_plugins,
        enabled_agents,
        missing_env,
    }))
}

// ============================================================================
// Internal helpers
// ============================================================================

fn load_preset(path: &PathBuf) -> anyhow::Result<PresetFile> {
    let contents = std::fs::read_to_string(path)?;
    let file: PresetFile = serde_yaml::from_str(&contents)?;
    Ok(file)
}

pub fn find_preset_file(presets_dir: &PathBuf, id: &str) -> anyhow::Result<Option<PathBuf>> {
    if !presets_dir.exists() {
        return Ok(None);
    }

    let entries = std::fs::read_dir(presets_dir)?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if !path.is_file() {
            continue;
        }

        if let Some(ext) = path.extension() {
            if ext != "yaml" && ext != "yml" {
                continue;
            }
        } else {
            continue;
        }

        match load_preset(&path) {
            Ok(preset_file) => {
                if preset_file.preset.id == id {
                    return Ok(Some(path));
                }
            }
            Err(_) => continue,
        }
    }

    Ok(None)
}

async fn set_agent_enabled(
    state: &ManagementState,
    id: &str,
    enabled: bool,
) -> anyhow::Result<()> {
    let agent_path = super::agents::find_agent_file(&state.agents_dir, id)?
        .ok_or_else(|| anyhow::anyhow!("Agent '{}' not found", id))?;

    let mut agent = super::agents::load_agent_definition(&agent_path)?;
    agent.enabled = enabled;

    super::agents::save_agent_definition(&agent_path, &agent)?;
    Ok(())
}

