use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::{error, info};

use crate::management::state::ManagementState;

// ============================================================================
// Request/Response types
// ============================================================================

#[derive(Serialize, Debug, Clone)]
pub struct PackageInfo {
    pub name: String,
    pub version: String,
    pub kind: String,
    pub description: Option<String>,
    pub author: Option<String>,
    pub source: String,
    pub installed: bool,
}

#[derive(Serialize, Debug)]
pub struct SearchResult {
    pub packages: Vec<PackageInfo>,
}

#[derive(Deserialize, Debug)]
pub struct SearchQuery {
    pub q: Option<String>,
    pub kind: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct InstallRequest {
    pub package: String,
    pub confirm: bool,
}

#[derive(Serialize, Debug, Clone)]
pub struct EnvVarInfo {
    pub key: String,
    pub label: String,
    pub help: String,
    pub required: bool,
}

#[derive(Serialize, Debug)]
pub struct InstallPreview {
    pub name: String,
    pub version: String,
    pub kind: String,
    pub description: Option<String>,
    pub author: Option<String>,
    pub plugins_to_install: Vec<String>,
    pub agents_to_install: Vec<String>,
    pub env_required: Vec<EnvVarInfo>,
    pub events_consumed: Vec<String>,
    pub events_produced: Vec<String>,
}

#[derive(Serialize, Debug)]
pub struct InstallResult {
    pub success: bool,
    pub message: String,
    pub installed: Vec<String>,
}

#[derive(Serialize, Debug)]
pub struct InstalledPackages {
    pub packages: Vec<InstalledPackageInfo>,
}

#[derive(Serialize, Debug, Clone)]
pub struct InstalledPackageInfo {
    pub name: String,
    pub version: String,
    pub kind: String,
    pub installed_at: String,
}

// ============================================================================
// Internal types for parsing registry index
// ============================================================================

#[derive(Deserialize, Debug)]
struct RegistryIndexResponse {
    packages: Option<Vec<RegistryEntryResponse>>,
}

#[derive(Deserialize, Debug, Clone)]
struct RegistryEntryResponse {
    name: String,
    latest: String,
    kind: String,
    description: Option<String>,
    author: Option<String>,
}

#[derive(Deserialize, Debug)]
struct InstalledYaml {
    packages: Option<Vec<InstalledEntry>>,
}

#[derive(Deserialize, Debug, Clone)]
struct InstalledEntry {
    name: String,
    version: String,
    kind: String,
    installed_at: Option<String>,
}

// ============================================================================
// Handlers
// ============================================================================

/// Search available packages across all registries
pub async fn search_packages(
    _state: State<ManagementState>,
    Query(query): Query<SearchQuery>,
) -> impl IntoResponse {
    info!("Searching packages with query: {:?}", query);

    match load_all_registries() {
        Ok(mut results) => {
            let q = query.q.as_deref().unwrap_or("");
            let kind_filter = query.kind.as_deref();

            results.retain(|pkg| {
                let matches_query = q.is_empty()
                    || pkg.name.to_lowercase().contains(&q.to_lowercase())
                    || pkg
                        .description
                        .as_ref()
                        .map(|d| d.to_lowercase().contains(&q.to_lowercase()))
                        .unwrap_or(false);

                let matches_kind = kind_filter.map(|k| k == pkg.kind).unwrap_or(true);

                matches_query && matches_kind
            });

            let installed = load_installed().unwrap_or_default();
            let installed_names: Vec<String> = installed.iter().map(|p| p.name.clone()).collect();

            for pkg in &mut results {
                pkg.installed = installed_names.contains(&pkg.name);
            }

            Json(SearchResult { packages: results }).into_response()
        }
        Err(e) => {
            error!("Failed to load registries: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to load registries: {}", e),
            )
                .into_response()
        }
    }
}

/// Install a package (or preview what would be installed)
pub async fn install_package(
    State(state): State<ManagementState>,
    Json(req): Json<InstallRequest>,
) -> impl IntoResponse {
    info!("Package install request: {:?}", req);

    let payload = match serde_json::to_vec(&req) {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to serialize install request: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                format!("Failed to serialize request: {}", e),
            )
                .into_response();
        }
    };

    match state
        .nats
        .request_bytes("daemon.pkg.install".to_string(), payload)
        .await
    {
        Ok(response) => {
            let body = String::from_utf8_lossy(&response);
            info!("Install response: {}", body);
            (StatusCode::OK, body.to_string()).into_response()
        }
        Err(e) => {
            error!("Failed to send install request to daemon: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Install failed: {}", e),
            )
                .into_response()
        }
    }
}

/// Uninstall a package
pub async fn uninstall_package(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    info!("Package uninstall request: {}", name);

    let payload = match serde_json::to_vec(&serde_json::json!({ "name": name })) {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to serialize uninstall request: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                format!("Failed to serialize request: {}", e),
            )
                .into_response();
        }
    };

    match state
        .nats
        .request_bytes("daemon.pkg.uninstall".to_string(), payload)
        .await
    {
        Ok(response) => {
            let body = String::from_utf8_lossy(&response);
            info!("Uninstall response: {}", body);
            (StatusCode::OK, body.to_string()).into_response()
        }
        Err(e) => {
            error!("Failed to send uninstall request to daemon: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Uninstall failed: {}", e),
            )
                .into_response()
        }
    }
}

/// List installed packages
pub async fn list_installed(_state: State<ManagementState>) -> impl IntoResponse {
    info!("Listing installed packages");

    match load_installed() {
        Ok(packages) => {
            let installed: Vec<InstalledPackageInfo> = packages
                .iter()
                .map(|p| InstalledPackageInfo {
                    name: p.name.clone(),
                    version: p.version.clone(),
                    kind: p.kind.clone(),
                    installed_at: p.installed_at.clone().unwrap_or_default(),
                })
                .collect();

            Json(InstalledPackages {
                packages: installed,
            })
            .into_response()
        }
        Err(_) => {
            // No installed packages yet
            Json(InstalledPackages {
                packages: Vec::new(),
            })
            .into_response()
        }
    }
}

// ============================================================================
// Private helpers
// ============================================================================

/// Load all packages from all configured registries
fn load_all_registries() -> anyhow::Result<Vec<PackageInfo>> {
    let mut all_packages = Vec::new();

    let registries_dir = get_registries_dir();
    if !registries_dir.exists() {
        return Ok(Vec::new());
    }

    if let Ok(entries) = std::fs::read_dir(&registries_dir) {
        for entry in entries.flatten() {
            let index_path = entry.path().join("index.yaml");
            if index_path.exists() {
                if let Ok(contents) = std::fs::read_to_string(&index_path) {
                    if let Ok(index) = serde_yaml::from_str::<RegistryIndexResponse>(&contents) {
                        let source = entry.file_name().to_string_lossy().to_string();

                        if let Some(packages) = index.packages {
                            for pkg in packages {
                                all_packages.push(PackageInfo {
                                    name: pkg.name,
                                    version: pkg.latest,
                                    kind: pkg.kind,
                                    description: pkg.description,
                                    author: pkg.author,
                                    source: source.clone(),
                                    installed: false,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(all_packages)
}

/// Load installed packages from ~/.seidrum/installed.yaml
fn load_installed() -> anyhow::Result<Vec<InstalledEntry>> {
    let installed_path = get_installed_path();
    if !installed_path.exists() {
        return Ok(Vec::new());
    }

    let contents = std::fs::read_to_string(&installed_path)?;
    let yaml = serde_yaml::from_str::<InstalledYaml>(&contents)?;

    Ok(yaml.packages.unwrap_or_default())
}

/// Get ~/.seidrum directory
fn get_seidrum_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".seidrum")
}

/// Get ~/.seidrum/registries directory
fn get_registries_dir() -> PathBuf {
    get_seidrum_dir().join("registries")
}

/// Get ~/.seidrum/installed.yaml path
fn get_installed_path() -> PathBuf {
    get_seidrum_dir().join("installed.yaml")
}
