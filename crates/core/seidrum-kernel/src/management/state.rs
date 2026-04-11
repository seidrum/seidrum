use std::path::PathBuf;

/// Shared state available to all management API handlers.
#[derive(Clone)]
pub struct ManagementState {
    pub nats: seidrum_common::bus_client::BusClient,
    pub config_dir: PathBuf,
    pub agents_dir: PathBuf,
    pub workflows_dir: PathBuf,
    pub env_file: PathBuf,
    pub presets_dir: PathBuf,
}

impl ManagementState {
    pub fn new(
        nats: seidrum_common::bus_client::BusClient,
        config_dir: PathBuf,
        agents_dir: PathBuf,
        workflows_dir: PathBuf,
        env_file: PathBuf,
    ) -> Self {
        let presets_dir = config_dir.join("presets");
        Self {
            nats,
            config_dir,
            agents_dir,
            workflows_dir,
            env_file,
            presets_dir,
        }
    }

    /// Path to plugins.yaml
    pub fn plugins_yaml(&self) -> PathBuf {
        self.config_dir.join("plugins.yaml")
    }
}
