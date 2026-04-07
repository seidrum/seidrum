//! Centralized path resolution for the daemon.

use std::path::PathBuf;

/// All filesystem paths used by the daemon.
pub struct SeidrumPaths {
    /// Directory containing plugin binaries.
    pub bin_dir: PathBuf,
    /// Directory containing config files (platform.yaml, plugins.yaml).
    pub config_dir: PathBuf,
    /// Directory for PID files and metadata.
    pub pid_dir: PathBuf,
    /// Directory for process log files.
    pub log_dir: PathBuf,
    /// Seidrum home directory (~/.seidrum/).
    pub seidrum_home: PathBuf,
}

impl SeidrumPaths {
    /// Resolve all paths based on the config directory.
    pub fn resolve(config_dir: &std::path::Path) -> Self {
        let bin_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));

        let home_dir = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));

        let seidrum_dir = home_dir.join(".seidrum");

        Self {
            bin_dir,
            config_dir: config_dir.to_path_buf(),
            pid_dir: seidrum_dir.join("pids"),
            log_dir: seidrum_dir.join("logs"),
            seidrum_home: seidrum_dir,
        }
    }

    /// Ensure all directories exist.
    pub fn ensure_dirs(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.pid_dir)?;
        std::fs::create_dir_all(&self.log_dir)?;
        std::fs::create_dir_all(self.managed_bin_dir())?;
        std::fs::create_dir_all(self.nats_data_dir())?;
        std::fs::create_dir_all(self.packages_dir())?;
        std::fs::create_dir_all(self.registries_dir())?;
        Ok(())
    }

    /// Directory for managed binaries (~/.seidrum/bin/).
    pub fn managed_bin_dir(&self) -> PathBuf {
        self.seidrum_home.join("bin")
    }

    /// Directory for persistent data (~/.seidrum/data/).
    pub fn data_dir(&self) -> PathBuf {
        self.seidrum_home.join("data")
    }

    /// NATS JetStream data directory (~/.seidrum/data/nats/).
    pub fn nats_data_dir(&self) -> PathBuf {
        self.seidrum_home.join("data").join("nats")
    }

    /// Infrastructure config file (~/.seidrum/infra.yaml).
    pub fn infra_config(&self) -> PathBuf {
        self.seidrum_home.join("infra.yaml")
    }

    /// NATS server PID file.
    pub fn nats_pid_file(&self) -> PathBuf {
        self.pid_dir.join("nats-server.pid")
    }

    /// ArangoDB container ID file.
    pub fn arango_container_id_file(&self) -> PathBuf {
        self.pid_dir.join("arangodb.container-id")
    }

    /// Path to the daemon's own PID file.
    pub fn daemon_pid_file(&self) -> PathBuf {
        self.pid_dir.join("seidrum.pid")
    }

    /// Path to a plugin's PID file.
    pub fn plugin_pid_file(&self, name: &str) -> PathBuf {
        self.pid_dir.join(format!("{}.pid", name))
    }

    /// Path to a plugin's metadata file.
    pub fn plugin_meta_file(&self, name: &str) -> PathBuf {
        self.pid_dir.join(format!("{}.meta", name))
    }

    /// Path to a plugin's stopped marker file.
    pub fn plugin_stopped_file(&self, name: &str) -> PathBuf {
        self.pid_dir.join(format!("{}.stopped", name))
    }

    /// Path to a plugin's log file.
    pub fn plugin_log_file(&self, name: &str) -> PathBuf {
        self.log_dir.join(format!("{}.log", name))
    }

    /// Resolve a plugin binary path. Checks the daemon's own directory first
    /// (for built-in plugins), then ~/.seidrum/bin/ (for installed packages).
    pub fn plugin_binary(&self, binary_name: &str) -> PathBuf {
        let builtin = self.bin_dir.join(binary_name);
        if builtin.exists() {
            return builtin;
        }
        let installed = self.managed_bin_dir().join(binary_name);
        if installed.exists() {
            return installed;
        }
        // Fall back to built-in path (will fail at spawn time with clear error)
        builtin
    }

    /// Directory for packages (~/.seidrum/packages/).
    pub fn packages_dir(&self) -> PathBuf {
        self.seidrum_home.join("packages")
    }

    /// Directory for registries (~/.seidrum/registries/).
    pub fn registries_dir(&self) -> PathBuf {
        self.seidrum_home.join("registries")
    }

    /// Path to installed.yaml (~/.seidrum/installed.yaml).
    pub fn installed_yaml(&self) -> PathBuf {
        self.seidrum_home.join("installed.yaml")
    }

    /// Path to plugins.yaml.
    pub fn plugins_yaml(&self) -> PathBuf {
        self.config_dir.join("plugins.yaml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_file_paths() {
        let paths = SeidrumPaths {
            bin_dir: PathBuf::from("/usr/bin"),
            config_dir: PathBuf::from("/etc/seidrum"),
            pid_dir: PathBuf::from("/var/run/seidrum"),
            log_dir: PathBuf::from("/var/log/seidrum"),
            seidrum_home: PathBuf::from("/home/test/.seidrum"),
        };

        assert_eq!(
            paths.plugin_pid_file("telegram"),
            PathBuf::from("/var/run/seidrum/telegram.pid")
        );
        assert_eq!(
            paths.plugin_binary("seidrum-telegram"),
            PathBuf::from("/usr/bin/seidrum-telegram")
        );
    }
}
