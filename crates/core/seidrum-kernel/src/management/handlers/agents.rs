use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};
use seidrum_common::config::AgentDefinition;
use std::path::PathBuf;
use tracing::{error, info};

use crate::management::state::ManagementState;

// ============================================================================
// Response types
// ============================================================================

#[derive(Debug, Serialize, Clone)]
pub struct AgentListItem {
    pub id: String,
    pub name: Option<String>,
    pub enabled: bool,
    pub scope: String,
    pub additional_scopes: Vec<String>,
    pub description: Option<String>,
    pub tools: Vec<String>,
    pub subscribe: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct AgentDetailResponse {
    pub id: String,
    pub prompt: String,
    pub tools: Vec<String>,
    pub scope: String,
    pub additional_scopes: Vec<String>,
    pub description: Option<String>,
    pub enabled: bool,
    pub subscribe: Vec<String>,
}

// ============================================================================
// Handlers
// ============================================================================

/// Scan agents directory, load each YAML, return list of agents.
pub async fn list_agents(
    State(state): State<ManagementState>,
) -> Result<Json<Vec<AgentListItem>>, (StatusCode, String)> {
    let agents_dir = &state.agents_dir;

    let entries = std::fs::read_dir(agents_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut agents = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                error!("Error reading agent directory entry: {}", e);
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

        match load_agent_definition(&path) {
            Ok(agent) => {
                agents.push(AgentListItem {
                    id: agent.id.clone(),
                    name: None, // Not in v2 format, but available in structure
                    enabled: agent.enabled,
                    scope: agent.scope.clone(),
                    additional_scopes: agent.additional_scopes.clone(),
                    description: agent.description.clone(),
                    tools: agent.tools.clone(),
                    subscribe: agent.subscribe.clone(),
                });
            }
            Err(e) => {
                error!("Error loading agent from {:?}: {}", path, e);
            }
        }
    }

    agents.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Json(agents))
}

/// Get a specific agent by id.
pub async fn get_agent(
    State(state): State<ManagementState>,
    Path(id): Path<String>,
) -> Result<Json<AgentDetailResponse>, (StatusCode, String)> {
    let agent_path = find_agent_file(&state.agents_dir, &id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("Agent '{}' not found", id),
        ))?;

    let agent = load_agent_definition(&agent_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(AgentDetailResponse {
        id: agent.id,
        prompt: agent.prompt,
        tools: agent.tools,
        scope: agent.scope,
        additional_scopes: agent.additional_scopes,
        description: agent.description,
        enabled: agent.enabled,
        subscribe: agent.subscribe,
    }))
}

/// Enable an agent by id.
pub async fn enable_agent(
    State(state): State<ManagementState>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<AgentDetailResponse>), (StatusCode, String)> {
    set_agent_enabled(&state, &id, true)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    info!("Agent '{}' enabled", id);

    // Publish reload signal to NATS
    if let Err(e) = state
        .nats
        .publish("kernel.agents.reload".to_string(), "".into())
        .await
    {
        tracing::error!("Failed to publish agent reload signal: {}", e);
    }

    let response = get_agent(State(state), Path(id))
        .await?;
    Ok((StatusCode::ACCEPTED, response))
}

/// Disable an agent by id.
pub async fn disable_agent(
    State(state): State<ManagementState>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<AgentDetailResponse>), (StatusCode, String)> {
    set_agent_enabled(&state, &id, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    info!("Agent '{}' disabled", id);

    // Publish reload signal to NATS
    if let Err(e) = state
        .nats
        .publish("kernel.agents.reload".to_string(), "".into())
        .await
    {
        tracing::error!("Failed to publish agent reload signal: {}", e);
    }

    let response = get_agent(State(state), Path(id))
        .await?;
    Ok((StatusCode::ACCEPTED, response))
}

// ============================================================================
// Internal helpers
// ============================================================================

pub fn load_agent_definition(path: &PathBuf) -> anyhow::Result<AgentDefinition> {
    let contents = std::fs::read_to_string(path)?;
    let file: seidrum_common::config::AgentDefinitionFile =
        serde_yaml::from_str(&contents)?;
    Ok(file.agent)
}

pub fn save_agent_definition(
    path: &PathBuf,
    agent: &AgentDefinition,
) -> anyhow::Result<()> {
    let file = seidrum_common::config::AgentDefinitionFile {
        agent: agent.clone(),
    };
    let contents = serde_yaml::to_string(&file)?;
    std::fs::write(path, contents)?;
    Ok(())
}

pub fn find_agent_file(agents_dir: &PathBuf, id: &str) -> anyhow::Result<Option<PathBuf>> {
    let entries = std::fs::read_dir(agents_dir)?;

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

        match load_agent_definition(&path) {
            Ok(agent) => {
                if agent.id == id {
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
    let agent_path = find_agent_file(&state.agents_dir, id)?
        .ok_or_else(|| anyhow::anyhow!("Agent '{}' not found", id))?;

    let mut agent = load_agent_definition(&agent_path)?;
    agent.enabled = enabled;

    save_agent_definition(&agent_path, &agent)?;
    Ok(())
}
