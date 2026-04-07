use axum::{
    extract::State,
    http::StatusCode,
    response::Json,
};
use serde::Serialize;
use tracing::info;

use crate::management::state::ManagementState;

// ============================================================================
// Response types
// ============================================================================

#[derive(Debug, Serialize)]
pub struct OnboardingStatusResponse {
    pub first_run: bool,
    pub plugins_enabled: usize,
    pub agents_enabled: usize,
    pub channels_active: usize,
}

#[derive(Debug, Serialize)]
pub struct OnboardingCompleteResponse {
    pub completed: bool,
}

// ============================================================================
// Handlers
// ============================================================================

/// Check onboarding status:
/// - first_run: true if .onboarding_complete marker doesn't exist
/// - plugins_enabled: count of enabled plugins in plugins.yaml
/// - agents_enabled: count of enabled agents
/// - channels_active: count of active channel plugins
pub async fn onboarding_status(
    State(state): State<ManagementState>,
) -> Result<Json<OnboardingStatusResponse>, (StatusCode, String)> {
    let onboarding_marker = state.config_dir.join(".onboarding_complete");
    let first_run = !onboarding_marker.exists();

    // Count enabled plugins
    let plugins_enabled = count_enabled_plugins(&state)
        .await
        .unwrap_or(0);

    // Count enabled agents
    let agents_enabled = count_enabled_agents(&state)
        .await
        .unwrap_or(0);

    // Count active channels (plugins that are channel-related and enabled)
    let channels_active = count_active_channels(&state)
        .await
        .unwrap_or(0);

    Ok(Json(OnboardingStatusResponse {
        first_run,
        plugins_enabled,
        agents_enabled,
        channels_active,
    }))
}

/// Write .onboarding_complete marker file.
pub async fn onboarding_complete(
    State(state): State<ManagementState>,
) -> Result<Json<OnboardingCompleteResponse>, (StatusCode, String)> {
    let marker_file = state.config_dir.join(".onboarding_complete");

    std::fs::write(&marker_file, "")
        .map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;

    info!("Onboarding marked as complete");

    Ok(Json(OnboardingCompleteResponse {
        completed: true,
    }))
}

// ============================================================================
// Internal helpers
// ============================================================================

async fn count_enabled_plugins(state: &ManagementState) -> anyhow::Result<usize> {
    let config = super::plugins::load_plugins_config(&state.plugins_yaml())?;
    let count = config
        .plugins
        .values()
        .filter(|entry| entry.enabled)
        .count();
    Ok(count)
}

async fn count_enabled_agents(state: &ManagementState) -> anyhow::Result<usize> {
    let agents_dir = &state.agents_dir;
    let entries = std::fs::read_dir(agents_dir)?;

    let mut count = 0;
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

        if let Ok(agent) = super::agents::load_agent_definition(&path) {
            if agent.enabled {
                count += 1;
            }
        }
    }

    Ok(count)
}

async fn count_active_channels(state: &ManagementState) -> anyhow::Result<usize> {
    let config = super::plugins::load_plugins_config(&state.plugins_yaml())?;

    // Channel plugins: telegram, cli, email, notification, api-gateway
    let channel_plugins = [
        "telegram",
        "cli",
        "email",
        "notification",
        "api-gateway",
    ];

    let count = config
        .plugins
        .iter()
        .filter(|(name, entry)| {
            entry.enabled && channel_plugins.contains(&name.as_str())
        })
        .count();

    Ok(count)
}
