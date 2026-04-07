use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};
use seidrum_common::config::{Preset, PresetBundle, PresetFile};
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::{error, info, warn};

use crate::management::state::ManagementState;

// ============================================================================
// Request/Response types
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct ImportBundleRequest {
    /// Source type: "inline" or "url"
    pub source: String,

    /// For source="inline": preset YAML content
    #[serde(default)]
    pub preset_yaml: String,

    /// For source="inline": map of agent filenames to content
    #[serde(default)]
    pub agents: HashMap<String, String>,

    /// For source="inline": map of prompt filenames to content
    #[serde(default)]
    pub prompts: HashMap<String, String>,

    /// For source="url": URL to download bundle from
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct InstallResult {
    pub preset_id: String,
    pub preset_name: String,
    pub version: Option<String>,
    pub installed_agents: Vec<String>,
    pub installed_prompts: Vec<String>,
    pub installed_plugins: Vec<String>,
    pub skipped_agents: Vec<String>,
    pub skipped_prompts: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ExportBundleResponse {
    pub preset: Preset,
    pub agents: HashMap<String, String>,
    pub prompts: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct PresetListItemWithSource {
    pub id: String,
    pub name: String,
    pub description: String,
    pub icon: String,
    pub version: Option<String>,
    pub author: Option<String>,
    pub repository: Option<String>,
    pub source: String, // "builtin" | "imported" | "custom"
    pub required_plugins: usize,
    pub recommended_plugins: usize,
    pub required_agents: usize,
}

// ============================================================================
// Handlers
// ============================================================================

/// Import a preset bundle from inline JSON or URL.
pub async fn import_preset(
    State(state): State<ManagementState>,
    Json(req): Json<ImportBundleRequest>,
) -> Result<Json<InstallResult>, (StatusCode, String)> {
    match req.source.as_str() {
        "inline" => {
            import_inline(&state, req).await.map(Json)
        }
        "url" => {
            // URL import would require HTTP client; for now return error
            Err((
                StatusCode::NOT_IMPLEMENTED,
                "URL import not yet implemented. Use inline import with preset YAML.".to_string(),
            ))
        }
        _ => Err((
            StatusCode::BAD_REQUEST,
            format!("Unknown import source: {}", req.source),
        )),
    }
}

/// Export a preset bundle as JSON.
pub async fn export_preset(
    State(state): State<ManagementState>,
    Path(id): Path<String>,
) -> Result<Json<ExportBundleResponse>, (StatusCode, String)> {
    let preset_path = super::presets::find_preset_file(&state.presets_dir, &id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("Preset '{}' not found", id),
        ))?;

    let preset_file = load_preset_file(&preset_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut agents = HashMap::new();
    let mut prompts = HashMap::new();

    if let Some(bundle) = &preset_file.preset.bundle {
        // Collect agent files
        for agent_file in &bundle.agents {
            let path = state.agents_dir.join(agent_file);
            if path.exists() {
                match std::fs::read_to_string(&path) {
                    Ok(content) => {
                        agents.insert(agent_file.clone(), content);
                    }
                    Err(e) => {
                        warn!("Could not read agent file {}: {}", agent_file, e);
                    }
                }
            }
        }

        // Collect prompt files
        for prompt_file in &bundle.prompts {
            let prompts_dir = state.config_dir.parent().unwrap_or(&state.config_dir).join("prompts");
            let path = prompts_dir.join(prompt_file);
            if path.exists() {
                match std::fs::read_to_string(&path) {
                    Ok(content) => {
                        prompts.insert(prompt_file.clone(), content);
                    }
                    Err(e) => {
                        warn!("Could not read prompt file {}: {}", prompt_file, e);
                    }
                }
            }
        }
    }

    Ok(Json(ExportBundleResponse {
        preset: preset_file.preset,
        agents,
        prompts,
    }))
}

/// Delete a custom preset.
pub async fn delete_preset(
    State(state): State<ManagementState>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let preset_path = super::presets::find_preset_file(&state.presets_dir, &id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("Preset '{}' not found", id),
        ))?;

    std::fs::remove_file(&preset_path)
        .map_err(|e| {
            error!("Failed to delete preset file: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;

    info!("Deleted preset '{}'", id);
    Ok(StatusCode::OK)
}

/// List all presets with source information.
pub async fn list_presets_with_source(
    State(state): State<ManagementState>,
) -> Result<Json<Vec<PresetListItemWithSource>>, (StatusCode, String)> {
    let presets_dir = &state.presets_dir;

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

        match load_preset_file(&path) {
            Ok(preset_file) => {
                let preset = &preset_file.preset;
                // Determine source: builtin presets are in the original config,
                // bundled ones have a version field and belong to the bundle
                let source = if preset.version.is_some() {
                    "imported".to_string()
                } else if preset.author.as_deref() == Some("seidrum") {
                    "builtin".to_string()
                } else {
                    "custom".to_string()
                };

                presets.push(PresetListItemWithSource {
                    id: preset.id.clone(),
                    name: preset.name.clone(),
                    description: preset.description.clone(),
                    icon: preset.icon.clone(),
                    version: preset.version.clone(),
                    author: preset.author.clone(),
                    repository: preset.repository.clone(),
                    source,
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

// ============================================================================
// Internal helpers
// ============================================================================

async fn import_inline(
    state: &ManagementState,
    req: ImportBundleRequest,
) -> Result<InstallResult, (StatusCode, String)> {
    // Parse preset YAML
    let preset_file: PresetFile = serde_yaml::from_str(&req.preset_yaml).map_err(|e| {
        error!("Failed to parse preset YAML: {}", e);
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid preset YAML: {}", e),
        )
    })?;

    let preset = &preset_file.preset;

    // Validate preset has required fields
    if preset.id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Preset must have an id".to_string(),
        ));
    }

    install_bundle(state, preset, &req.agents, &req.prompts)
        .await
        .map_err(|e| {
            error!("Failed to install bundle: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to install bundle: {}", e),
            )
        })
}

async fn install_bundle(
    state: &ManagementState,
    preset: &Preset,
    agents: &HashMap<String, String>,
    prompts: &HashMap<String, String>,
) -> anyhow::Result<InstallResult> {
    let mut installed_agents = Vec::new();
    let mut installed_prompts = Vec::new();
    let mut installed_plugins = Vec::new();
    let mut skipped_agents = Vec::new();
    let mut skipped_prompts = Vec::new();

    // 1. Install agent YAMLs if bundle is defined
    if let Some(bundle) = &preset.bundle {
        for agent_file in &bundle.agents {
            if let Some(content) = agents.get(agent_file) {
                let dest = state.agents_dir.join(agent_file);

                // Don't overwrite existing agents
                if dest.exists() {
                    warn!(
                        "Agent file {} already exists, skipping",
                        agent_file
                    );
                    skipped_agents.push(agent_file.clone());
                    continue;
                }

                std::fs::write(&dest, content).map_err(|e| {
                    anyhow::anyhow!("Failed to write agent file {}: {}", agent_file, e)
                })?;

                installed_agents.push(agent_file.clone());
            }
        }

        // 2. Install prompt files
        let prompts_dir = state
            .config_dir
            .parent()
            .unwrap_or(&state.config_dir)
            .join("prompts");

        // Create prompts dir if needed
        if !prompts_dir.exists() {
            std::fs::create_dir_all(&prompts_dir)?;
        }

        for prompt_file in &bundle.prompts {
            if let Some(content) = prompts.get(prompt_file) {
                let dest = prompts_dir.join(prompt_file);

                if dest.exists() {
                    warn!(
                        "Prompt file {} already exists, skipping",
                        prompt_file
                    );
                    skipped_prompts.push(prompt_file.clone());
                    continue;
                }

                std::fs::write(&dest, content).map_err(|e| {
                    anyhow::anyhow!("Failed to write prompt file {}: {}", prompt_file, e)
                })?;

                installed_prompts.push(prompt_file.clone());
            }
        }

        // 3. Add new plugin entries to plugins.yaml
        let plugins_yaml = state.plugins_yaml();
        let mut plugins_config =
            match super::plugins::load_plugins_config(&plugins_yaml) {
                Ok(cfg) => cfg,
                Err(_) => super::plugins::PluginsConfig {
                    plugins: std::collections::BTreeMap::new(),
                },
            };

        for bundled_plugin in &bundle.plugins {
            // Only add if not already present
            if !plugins_config.plugins.contains_key(&bundled_plugin.name) {
                plugins_config.plugins.insert(
                    bundled_plugin.name.clone(),
                    super::plugins::PluginEntry {
                        binary: bundled_plugin.binary.clone(),
                        enabled: false, // Start disabled; user must enable
                        env: bundled_plugin.env.clone(),
                    },
                );
                installed_plugins.push(bundled_plugin.name.clone());
            }
        }

        if !installed_plugins.is_empty() {
            super::plugins::save_plugins_config(&plugins_yaml, &plugins_config)?;
        }
    }

    // 4. Copy preset.yaml to config/presets/
    let preset_dest = state.presets_dir.join(format!("{}.yaml", preset.id));

    // Create presets dir if needed
    if !state.presets_dir.exists() {
        std::fs::create_dir_all(&state.presets_dir)?;
    }

    let preset_file = PresetFile { preset: preset.clone() };
    let yaml = serde_yaml::to_string(&preset_file)?;
    std::fs::write(&preset_dest, yaml)?;

    info!(
        "Installed preset '{}' ({} agents, {} prompts, {} plugins)",
        preset.id,
        installed_agents.len(),
        installed_prompts.len(),
        installed_plugins.len()
    );

    Ok(InstallResult {
        preset_id: preset.id.clone(),
        preset_name: preset.name.clone(),
        version: preset.version.clone(),
        installed_agents,
        installed_prompts,
        installed_plugins,
        skipped_agents,
        skipped_prompts,
    })
}

fn load_preset_file(path: &PathBuf) -> anyhow::Result<PresetFile> {
    let contents = std::fs::read_to_string(path)?;
    let file: PresetFile = serde_yaml::from_str(&contents)?;
    Ok(file)
}
