use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};
use tracing::{error, info};

use crate::management::state::ManagementState;

// ============================================================================
// Response types
// ============================================================================

#[derive(Debug, Serialize, Clone)]
pub struct EnvItem {
    pub key: String,
    pub has_value: bool,
}

#[derive(Debug, Deserialize)]
pub struct SetEnvRequest {
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct SetEnvResponse {
    pub key: String,
    pub set: bool,
}

// ============================================================================
// Handlers
// ============================================================================

/// Read .env file and return list of keys with whether they have values.
/// Never return actual secret values.
pub async fn list_env(
    State(state): State<ManagementState>,
) -> Result<Json<Vec<EnvItem>>, (StatusCode, String)> {
    let env_content = std::fs::read_to_string(&state.env_file)
        .unwrap_or_default();

    let items: Vec<EnvItem> = env_content
        .lines()
        .filter_map(|line| {
            let line = line.trim();

            // Skip empty lines and comments
            if line.is_empty() || line.starts_with('#') {
                return None;
            }

            // Parse KEY=VALUE
            if let Some(eq_pos) = line.find('=') {
                let key = line[..eq_pos].trim().to_string();
                let value = &line[eq_pos + 1..];
                Some(EnvItem {
                    key,
                    has_value: !value.trim().is_empty(),
                })
            } else {
                None
            }
        })
        .collect();

    Ok(Json(items))
}

/// Set or update an environment variable in .env file.
pub async fn set_env(
    State(state): State<ManagementState>,
    Path(key): Path<String>,
    Json(req): Json<SetEnvRequest>,
) -> Result<Json<SetEnvResponse>, (StatusCode, String)> {
    let env_file = &state.env_file;

    // Read existing .env content
    let content = std::fs::read_to_string(env_file)
        .unwrap_or_default();

    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();

    // Find or create the key
    let mut found = false;
    for line in &mut lines {
        let line_trimmed = line.trim();
        if line_trimmed.starts_with(&format!("{}=", key)) {
            // Replace existing key
            *line = format!("{}={}", key, req.value);
            found = true;
            break;
        }
    }

    if !found {
        // Add new key at the end
        lines.push(format!("{}={}", key, req.value));
    }

    // Write back to file
    let new_content = lines.join("\n") + "\n";
    std::fs::write(env_file, new_content)
        .map_err(|e| {
            error!("Failed to write .env file: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;

    info!("Environment variable '{}' set", key);

    Ok(Json(SetEnvResponse {
        key,
        set: true,
    }))
}
