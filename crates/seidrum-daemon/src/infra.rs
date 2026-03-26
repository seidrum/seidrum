//! Infrastructure lifecycle management: NATS (native binary) and ArangoDB (Docker container).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::paths::SeidrumPaths;

/// Pinned NATS server version.
pub const NATS_VERSION: &str = "2.10.24";

/// How NATS is managed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NatsMode {
    /// Managed native binary in ~/.seidrum/bin/.
    Native,
    /// Managed Docker container.
    Docker,
    /// User-managed external instance.
    External,
}

/// How ArangoDB is managed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ArangoMode {
    /// Managed Docker/Podman container.
    Docker,
    /// User-managed external instance.
    External,
}

/// Container runtime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ContainerRuntime {
    Docker,
    Podman,
}

/// NATS infrastructure configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NatsConfig {
    pub mode: NatsMode,
    #[serde(default = "default_nats_version")]
    pub version: String,
    #[serde(default = "default_nats_port")]
    pub port: u16,
    #[serde(default = "default_nats_http_port")]
    pub http_port: u16,
}

fn default_nats_version() -> String {
    NATS_VERSION.to_string()
}
fn default_nats_port() -> u16 {
    4222
}
fn default_nats_http_port() -> u16 {
    8222
}

/// ArangoDB infrastructure configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArangoConfig {
    pub mode: ArangoMode,
    #[serde(default = "default_arango_image")]
    pub image: String,
    #[serde(default = "default_arango_port")]
    pub port: u16,
    #[serde(default = "default_container_name")]
    pub container_name: String,
    #[serde(default = "default_volume_name")]
    pub volume_name: String,
    pub password: String,
}

fn default_arango_image() -> String {
    "arangodb:3.12".to_string()
}
fn default_arango_port() -> u16 {
    8529
}
fn default_container_name() -> String {
    "seidrum-arangodb".to_string()
}
fn default_volume_name() -> String {
    "seidrum-arango-data".to_string()
}

/// Full infrastructure configuration, persisted to ~/.seidrum/infra.yaml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfraConfig {
    pub nats: NatsConfig,
    pub arango: ArangoConfig,
    #[serde(default)]
    pub container_runtime: Option<ContainerRuntime>,
}

impl InfraConfig {
    /// Load from ~/.seidrum/infra.yaml, or return None if it doesn't exist.
    pub fn load(paths: &SeidrumPaths) -> Result<Option<Self>> {
        let path = paths.infra_config();
        if !path.exists() {
            return Ok(None);
        }
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let config: Self = serde_yaml::from_str(&contents)
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        Ok(Some(config))
    }

    /// Save to ~/.seidrum/infra.yaml.
    pub fn save(&self, paths: &SeidrumPaths) -> Result<()> {
        let path = paths.infra_config();
        let contents = serde_yaml::to_string(self).context("Failed to serialize infra config")?;
        std::fs::write(&path, contents)
            .with_context(|| format!("Failed to write {}", path.display()))?;
        Ok(())
    }
}

/// Manages infrastructure lifecycle (NATS and ArangoDB).
pub struct InfraManager {
    pub config: InfraConfig,
    paths: SeidrumPaths,
}

impl InfraManager {
    pub fn new(config: InfraConfig, paths: SeidrumPaths) -> Self {
        Self { config, paths }
    }

    /// Load infra config from disk, or return None if not configured yet.
    pub fn load(paths: &SeidrumPaths) -> Result<Option<Self>> {
        match InfraConfig::load(paths)? {
            Some(config) => Ok(Some(Self {
                config,
                paths: SeidrumPaths::resolve(&paths.config_dir),
            })),
            None => Ok(None),
        }
    }

    // -----------------------------------------------------------------------
    // Container runtime detection
    // -----------------------------------------------------------------------

    /// Detect whether Docker or Podman is available.
    pub fn detect_container_runtime() -> Option<ContainerRuntime> {
        if command_succeeds("docker", &["info"]) {
            Some(ContainerRuntime::Docker)
        } else if command_succeeds("podman", &["info"]) {
            Some(ContainerRuntime::Podman)
        } else {
            None
        }
    }

    fn runtime_cmd(&self) -> &str {
        match &self.config.container_runtime {
            Some(ContainerRuntime::Podman) => "podman",
            _ => "docker",
        }
    }

    // -----------------------------------------------------------------------
    // Port detection
    // -----------------------------------------------------------------------

    /// Check if a TCP port is in use by attempting to connect.
    pub fn is_port_in_use(port: u16) -> bool {
        std::net::TcpStream::connect(format!("127.0.0.1:{}", port)).is_ok()
    }

    // -----------------------------------------------------------------------
    // NATS management
    // -----------------------------------------------------------------------

    /// Download the NATS server binary to ~/.seidrum/bin/nats-server.
    pub async fn download_nats(&self) -> Result<()> {
        let dest = self.nats_binary_path();
        if dest.exists() {
            info!("NATS binary already exists at {}", dest.display());
            return Ok(());
        }

        let (os, arch) = platform_pair()?;
        let version = &self.config.nats.version;
        let url = format!(
            "https://github.com/nats-io/nats-server/releases/download/v{version}/nats-server-v{version}-{os}-{arch}.zip"
        );

        info!(%url, "Downloading NATS server");
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} {msg}")
                .expect("invalid template"),
        );
        pb.set_message(format!("Downloading NATS v{}...", version));
        pb.enable_steady_tick(Duration::from_millis(120));

        let response = reqwest::get(&url)
            .await
            .with_context(|| format!("Failed to download NATS from {}", url))?;

        if !response.status().is_success() {
            anyhow::bail!(
                "Failed to download NATS: HTTP {}. URL: {}",
                response.status(),
                url
            );
        }

        let bytes = response
            .bytes()
            .await
            .context("Failed to read NATS download")?;

        pb.set_message("Extracting...");

        // Extract nats-server binary from zip
        let cursor = std::io::Cursor::new(&bytes);
        let mut archive = zip::ZipArchive::new(cursor).context("Failed to open NATS zip")?;

        let mut found = false;
        for i in 0..archive.len() {
            let mut file = archive.by_index(i)?;
            let name = file.name().to_string();
            if name.ends_with("/nats-server") || name == "nats-server" {
                std::fs::create_dir_all(dest.parent().unwrap())?;
                let mut out = std::fs::File::create(&dest)?;
                std::io::copy(&mut file, &mut out)?;

                // Make executable
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
                }

                found = true;
                break;
            }
        }

        if !found {
            anyhow::bail!("nats-server binary not found in downloaded archive");
        }

        pb.finish_with_message(format!("NATS v{} installed to {}", version, dest.display()));
        Ok(())
    }

    fn nats_binary_path(&self) -> PathBuf {
        self.paths.managed_bin_dir().join("nats-server")
    }

    /// Start NATS server as a native child process.
    pub fn start_nats(&self) -> Result<()> {
        if self.config.nats.mode == NatsMode::External {
            info!("NATS mode is external, skipping start");
            return Ok(());
        }

        // Check if already running
        if Self::is_port_in_use(self.config.nats.port) {
            info!(port = self.config.nats.port, "NATS port already in use, assuming external");
            return Ok(());
        }

        let binary = self.nats_binary_path();
        if !binary.exists() {
            anyhow::bail!(
                "NATS binary not found at {}. Run 'seidrum setup' first.",
                binary.display()
            );
        }

        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.paths.log_dir.join("nats-server.log"))?;
        let log_stderr = log_file.try_clone()?;

        let child = std::process::Command::new(&binary)
            .arg("--jetstream")
            .arg("--store_dir")
            .arg(self.paths.nats_data_dir())
            .arg("--port")
            .arg(self.config.nats.port.to_string())
            .arg("--http_port")
            .arg(self.config.nats.http_port.to_string())
            .stdout(std::process::Stdio::from(log_file))
            .stderr(std::process::Stdio::from(log_stderr))
            .spawn()
            .context("Failed to start NATS server")?;

        let pid = child.id();
        std::fs::write(self.paths.nats_pid_file(), pid.to_string())?;

        info!(%pid, "NATS server started");
        Ok(())
    }

    /// Stop NATS server via PID file.
    pub fn stop_nats(&self) -> Result<()> {
        if self.config.nats.mode == NatsMode::External {
            return Ok(());
        }

        let pid_file = self.paths.nats_pid_file();
        if let Ok(pid_str) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                if crate::daemon::is_process_alive(pid) {
                    info!(%pid, "Stopping NATS server");
                    unsafe {
                        libc::kill(pid, libc::SIGTERM);
                    }
                    // Wait briefly for clean shutdown
                    for _ in 0..10 {
                        std::thread::sleep(Duration::from_millis(500));
                        if !crate::daemon::is_process_alive(pid) {
                            break;
                        }
                    }
                }
            }
            let _ = std::fs::remove_file(&pid_file);
        }
        Ok(())
    }

    /// Check if NATS is responsive.
    pub fn is_nats_running(&self) -> bool {
        Self::is_port_in_use(self.config.nats.port)
    }

    // -----------------------------------------------------------------------
    // ArangoDB management (Docker/Podman container)
    // -----------------------------------------------------------------------

    /// Pull the ArangoDB Docker image.
    pub fn pull_arango_image(&self) -> Result<()> {
        if self.config.arango.mode == ArangoMode::External {
            return Ok(());
        }

        let runtime = self.runtime_cmd();
        info!(image = %self.config.arango.image, "Pulling ArangoDB image");

        let status = std::process::Command::new(runtime)
            .args(["pull", &self.config.arango.image])
            .status()
            .with_context(|| format!("Failed to run '{} pull'", runtime))?;

        if !status.success() {
            anyhow::bail!("Failed to pull ArangoDB image");
        }
        Ok(())
    }

    /// Start ArangoDB as a Docker/Podman container.
    pub fn start_arango(&self) -> Result<()> {
        if self.config.arango.mode == ArangoMode::External {
            info!("ArangoDB mode is external, skipping start");
            return Ok(());
        }

        // Check if already running
        if Self::is_port_in_use(self.config.arango.port) {
            info!(port = self.config.arango.port, "ArangoDB port already in use, assuming external");
            return Ok(());
        }

        let runtime = self.runtime_cmd();
        let name = &self.config.arango.container_name;

        // Remove any existing stopped container with the same name
        let _ = std::process::Command::new(runtime)
            .args(["rm", "-f", name])
            .output();

        let output = std::process::Command::new(runtime)
            .args([
                "run",
                "-d",
                "--name",
                name,
                "-p",
                &format!("{}:8529", self.config.arango.port),
                "-v",
                &format!("{}:/var/lib/arangodb3", self.config.arango.volume_name),
                "-e",
                &format!("ARANGO_ROOT_PASSWORD={}", self.config.arango.password),
                &self.config.arango.image,
            ])
            .output()
            .with_context(|| format!("Failed to run '{} run'", runtime))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to start ArangoDB container: {}", stderr.trim());
        }

        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        std::fs::write(self.paths.arango_container_id_file(), &container_id)?;

        info!(container_id = %container_id, "ArangoDB container started");
        Ok(())
    }

    /// Stop and remove the ArangoDB container.
    pub fn stop_arango(&self) -> Result<()> {
        if self.config.arango.mode == ArangoMode::External {
            return Ok(());
        }

        let runtime = self.runtime_cmd();
        let name = &self.config.arango.container_name;

        info!(name = %name, "Stopping ArangoDB container");
        let _ = std::process::Command::new(runtime)
            .args(["stop", name])
            .output();
        let _ = std::process::Command::new(runtime)
            .args(["rm", name])
            .output();

        let _ = std::fs::remove_file(self.paths.arango_container_id_file());
        Ok(())
    }

    /// Check if ArangoDB container is running.
    pub fn is_arango_running(&self) -> bool {
        if self.config.arango.mode == ArangoMode::External {
            return Self::is_port_in_use(self.config.arango.port);
        }

        let runtime = self.runtime_cmd();
        let name = &self.config.arango.container_name;

        let output = std::process::Command::new(runtime)
            .args(["inspect", "-f", "{{.State.Running}}", name])
            .output();

        match output {
            Ok(o) if o.status.success() => {
                String::from_utf8_lossy(&o.stdout).trim() == "true"
            }
            _ => false,
        }
    }

    // -----------------------------------------------------------------------
    // Health checks
    // -----------------------------------------------------------------------

    /// Wait for NATS to become healthy.
    pub async fn wait_for_nats(&self) -> Result<()> {
        let url = format!("http://127.0.0.1:{}/healthz", self.config.nats.http_port);
        wait_for_http(&url, "NATS", 10, Duration::from_secs(1)).await
    }

    /// Wait for ArangoDB to become healthy.
    pub async fn wait_for_arango(&self) -> Result<()> {
        let url = format!("http://127.0.0.1:{}/_api/version", self.config.arango.port);
        wait_for_http(&url, "ArangoDB", 30, Duration::from_secs(2)).await
    }

    /// Wait for both NATS and ArangoDB to be healthy.
    pub async fn wait_for_healthy(&self) -> Result<()> {
        self.wait_for_nats().await?;
        self.wait_for_arango().await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Status
    // -----------------------------------------------------------------------

    /// Get NATS PID from the PID file, if managed.
    pub fn nats_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.paths.nats_pid_file())
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }

    /// Get ArangoDB container name, if managed.
    pub fn arango_container_name(&self) -> &str {
        &self.config.arango.container_name
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check if a command exits successfully (suppressing output).
fn command_succeeds(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Get the (os, arch) pair for NATS download URLs.
fn platform_pair() -> Result<(&'static str, &'static str)> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        other => anyhow::bail!("Unsupported OS for NATS download: {}", other),
    };

    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => anyhow::bail!("Unsupported architecture for NATS download: {}", other),
    };

    Ok((os, arch))
}

/// Wait for an HTTP endpoint to respond successfully.
async fn wait_for_http(url: &str, name: &str, max_retries: u32, interval: Duration) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;

    for attempt in 1..=max_retries {
        match client.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                info!(name, attempt, "Health check passed");
                return Ok(());
            }
            Ok(resp) => {
                info!(name, attempt, status = %resp.status(), "Health check: not ready");
            }
            Err(_) => {
                if attempt % 5 == 0 {
                    info!(name, attempt, max_retries, "Waiting for {} to start...", name);
                }
            }
        }
        tokio::time::sleep(interval).await;
    }

    anyhow::bail!(
        "{} did not become healthy after {} attempts. Check logs in ~/.seidrum/logs/",
        name,
        max_retries
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_pair_is_valid() {
        let result = platform_pair();
        assert!(result.is_ok(), "Should detect platform");
        let (os, arch) = result.unwrap();
        assert!(os == "darwin" || os == "linux");
        assert!(arch == "amd64" || arch == "arm64");
    }

    #[test]
    fn infra_config_serialization_roundtrip() {
        let config = InfraConfig {
            nats: NatsConfig {
                mode: NatsMode::Native,
                version: NATS_VERSION.to_string(),
                port: 4222,
                http_port: 8222,
            },
            arango: ArangoConfig {
                mode: ArangoMode::Docker,
                image: "arangodb:3.12".to_string(),
                port: 8529,
                container_name: "seidrum-arangodb".to_string(),
                volume_name: "seidrum-arango-data".to_string(),
                password: "test123".to_string(),
            },
            container_runtime: Some(ContainerRuntime::Docker),
        };

        let yaml = serde_yaml::to_string(&config).unwrap();
        let parsed: InfraConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.nats.mode, NatsMode::Native);
        assert_eq!(parsed.arango.mode, ArangoMode::Docker);
        assert_eq!(parsed.container_runtime, Some(ContainerRuntime::Docker));
    }

    #[test]
    fn command_succeeds_true() {
        assert!(command_succeeds("true", &[]));
    }

    #[test]
    fn command_succeeds_false() {
        assert!(!command_succeeds("false", &[]));
    }

    #[test]
    fn command_succeeds_nonexistent() {
        assert!(!command_succeeds("nonexistent-binary-xyz", &[]));
    }
}
