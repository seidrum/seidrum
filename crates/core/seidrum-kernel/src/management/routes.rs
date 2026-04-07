use axum::{
    extract::State,
    response::Json,
    routing::{delete, get, post, put},
    Router,
};
use serde::Serialize;

use super::handlers::{agents, bundles, config, onboarding, packages, plugins, presets};
use super::state::ManagementState;

pub fn build_router(state: ManagementState) -> Router {
    Router::new()
        // System
        .route("/api/mgmt/health", get(health))
        .route("/api/mgmt/status", get(system_status))
        // Plugins
        .route("/api/mgmt/plugins", get(plugins::list_plugins))
        .route(
            "/api/mgmt/plugins/:name/enable",
            put(plugins::enable_plugin),
        )
        .route(
            "/api/mgmt/plugins/:name/disable",
            put(plugins::disable_plugin),
        )
        .route("/api/mgmt/plugins/:name/start", post(plugins::start_plugin))
        .route("/api/mgmt/plugins/:name/stop", post(plugins::stop_plugin))
        .route(
            "/api/mgmt/plugins/:name/config/schema",
            get(plugins::plugin_config_schema),
        )
        // Agents
        .route("/api/mgmt/agents", get(agents::list_agents))
        .route("/api/mgmt/agents/:id", get(agents::get_agent))
        .route("/api/mgmt/agents/:id/enable", put(agents::enable_agent))
        .route("/api/mgmt/agents/:id/disable", put(agents::disable_agent))
        // Presets
        .route("/api/mgmt/presets", get(bundles::list_presets_with_source))
        .route("/api/mgmt/presets/:id/apply", post(presets::apply_preset))
        .route("/api/mgmt/presets/import", post(bundles::import_preset))
        .route("/api/mgmt/presets/:id/export", get(bundles::export_preset))
        .route("/api/mgmt/presets/:id", delete(bundles::delete_preset))
        // Configuration
        .route("/api/mgmt/config/env", get(config::list_env))
        .route("/api/mgmt/config/env/:key", put(config::set_env))
        // Packages
        .route("/api/mgmt/pkg/search", get(packages::search_packages))
        .route("/api/mgmt/pkg/install", post(packages::install_package))
        .route("/api/mgmt/pkg/:name", delete(packages::uninstall_package))
        .route("/api/mgmt/pkg/installed", get(packages::list_installed))
        // Onboarding
        .route(
            "/api/mgmt/onboarding/status",
            get(onboarding::onboarding_status),
        )
        .route(
            "/api/mgmt/onboarding/complete",
            post(onboarding::onboarding_complete),
        )
        .with_state(state)
}

#[derive(Serialize, Debug, Clone)]
struct HealthResponse {
    status: String,
    uptime_seconds: u64,
    nats_connected: bool,
}

async fn health(State(state): State<ManagementState>) -> Json<HealthResponse> {
    let nats_connected = state.nats.connection_state() == async_nats::connection::State::Connected;
    Json(HealthResponse {
        status: "ok".to_string(),
        uptime_seconds: 0, // TODO: track actual uptime
        nats_connected,
    })
}

#[derive(Serialize, Debug, Clone)]
struct SystemStatusResponse {
    status: String,
    timestamp: String,
}

async fn system_status(_state: State<ManagementState>) -> Json<SystemStatusResponse> {
    Json(SystemStatusResponse {
        status: "ok".to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
    })
}
