//! Process supervisor: spawn, monitor, restart, and gracefully shut down
//! the kernel and all enabled plugins.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use tokio::process::Command;
use tokio::signal;
use tracing::{error, info, warn};

use crate::config::{load_plugins_config, resolve_plugin_env, PluginEntry};
use crate::paths::SeidrumPaths;
use crate::status::ProcessMeta;

/// Maximum restart attempts within the backoff window.
const MAX_RESTARTS: u32 = 5;
/// Backoff window — reset restart count after this duration.
const BACKOFF_WINDOW: Duration = Duration::from_secs(300);
/// Grace period for SIGTERM before SIGKILL.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// A managed child process.
struct ManagedProcess {
    name: String,
    binary: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    child: Option<tokio::process::Child>,
    restart_count: u32,
    first_restart_at: Option<Instant>,
    started_at: Option<chrono::DateTime<Utc>>,
}

impl ManagedProcess {
    fn new(name: String, binary: String, args: Vec<String>, env: BTreeMap<String, String>) -> Self {
        Self {
            name,
            binary,
            args,
            env,
            child: None,
            restart_count: 0,
            first_restart_at: None,
            started_at: None,
        }
    }

    /// Spawn the child process.
    fn spawn(&mut self, paths: &SeidrumPaths) -> Result<()> {
        let binary_path = paths.plugin_binary(&self.binary);

        if !binary_path.exists() {
            anyhow::bail!(
                "Binary not found: {} (looked at {})",
                self.binary,
                binary_path.display()
            );
        }

        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(paths.plugin_log_file(&self.name))
            .with_context(|| format!("Failed to open log file for {}", self.name))?;

        let log_stderr = log_file
            .try_clone()
            .context("Failed to clone log file handle")?;

        let mut cmd = Command::new(&binary_path);
        cmd.args(&self.args);

        // Pass through parent environment + plugin-specific env vars
        for (k, v) in &self.env {
            cmd.env(k, v);
        }

        cmd.stdout(std::process::Stdio::from(log_file));
        cmd.stderr(std::process::Stdio::from(log_stderr));

        let child = cmd.spawn().with_context(|| {
            format!("Failed to spawn {} ({})", self.name, binary_path.display())
        })?;

        let pid = child.id().unwrap_or(0);
        self.started_at = Some(Utc::now());
        self.child = Some(child);

        // Write PID file
        let _ = std::fs::write(paths.plugin_pid_file(&self.name), pid.to_string());

        // Write metadata
        let meta = ProcessMeta {
            pid,
            started_at: self.started_at.unwrap(),
            restart_count: self.restart_count,
        };
        let _ = std::fs::write(
            paths.plugin_meta_file(&self.name),
            serde_json::to_string(&meta).unwrap_or_default(),
        );

        // Remove stopped marker if present
        let _ = std::fs::remove_file(paths.plugin_stopped_file(&self.name));

        info!(name = %self.name, %pid, "Process started");
        Ok(())
    }

    /// Check if restart should be attempted based on backoff policy.
    fn should_restart(&mut self) -> bool {
        let now = Instant::now();

        // Reset restart count if outside the backoff window
        if let Some(first) = self.first_restart_at {
            if now.duration_since(first) > BACKOFF_WINDOW {
                self.restart_count = 0;
                self.first_restart_at = None;
            }
        }

        if self.restart_count >= MAX_RESTARTS {
            return false;
        }

        if self.first_restart_at.is_none() {
            self.first_restart_at = Some(now);
        }

        self.restart_count += 1;
        true
    }

    /// Calculate backoff delay for the current restart attempt.
    fn backoff_delay(&self) -> Duration {
        let secs = 1u64 << self.restart_count.min(4); // 1, 2, 4, 8, 16
        Duration::from_secs(secs)
    }
}

/// Start the daemon: spawn kernel + all enabled plugins, enter monitoring loop.
pub async fn start(paths: &SeidrumPaths) -> Result<()> {
    paths.ensure_dirs()?;

    // Check if already running
    if let Ok(pid_str) = std::fs::read_to_string(paths.daemon_pid_file()) {
        if let Ok(pid) = pid_str.trim().parse::<i32>() {
            // Check if process is alive
            if unsafe { libc::kill(pid, 0) } == 0 {
                anyhow::bail!(
                    "Daemon is already running (PID {}). Use 'seidrum daemon stop' first.",
                    pid
                );
            }
        }
    }

    // Write our own PID
    std::fs::write(paths.daemon_pid_file(), std::process::id().to_string())?;

    info!("Starting Seidrum daemon");

    let mut processes: Vec<ManagedProcess> = Vec::new();

    // 1. Start the kernel
    let mut kernel = ManagedProcess::new(
        "kernel".to_string(),
        "seidrum-kernel".to_string(),
        vec!["serve".to_string()],
        BTreeMap::new(),
    );

    kernel.spawn(paths)?;
    processes.push(kernel);

    // Wait for kernel to connect to NATS
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 2. Load plugins.yaml and start enabled plugins
    let plugins_yaml = paths.plugins_yaml();
    if plugins_yaml.exists() {
        let config = load_plugins_config(&plugins_yaml)?;

        for (name, entry) in &config.plugins {
            if !entry.enabled {
                info!(name = %name, "Plugin disabled, skipping");
                continue;
            }

            // Skip if stopped marker exists
            if paths.plugin_stopped_file(name).exists() {
                info!(name = %name, "Plugin manually stopped, skipping");
                continue;
            }

            let env = resolve_plugin_env(entry);
            let mut proc = ManagedProcess::new(name.clone(), entry.binary.clone(), vec![], env);

            match proc.spawn(paths) {
                Ok(()) => processes.push(proc),
                Err(e) => error!(name = %name, error = %e, "Failed to start plugin"),
            }
        }
    } else {
        warn!(
            "No plugins.yaml found at {}. Only the kernel will run.",
            plugins_yaml.display()
        );
    }

    info!(
        count = processes.len(),
        "All processes started, entering monitoring loop"
    );

    // 3. Monitoring loop
    loop {
        tokio::select! {
            _ = signal::ctrl_c() => {
                info!("Received shutdown signal");
                break;
            }
            _ = check_processes(&mut processes, paths) => {
                // A process exited and was handled
            }
        }
    }

    // 4. Graceful shutdown
    shutdown(&mut processes, paths).await;

    // Clean up daemon PID
    let _ = std::fs::remove_file(paths.daemon_pid_file());

    info!("Seidrum daemon stopped");
    Ok(())
}

/// Monitor processes and restart crashed ones.
async fn check_processes(processes: &mut Vec<ManagedProcess>, paths: &SeidrumPaths) {
    // Small polling interval
    tokio::time::sleep(Duration::from_secs(1)).await;

    for proc in processes.iter_mut() {
        let child = match proc.child.as_mut() {
            Some(c) => c,
            None => continue,
        };

        // Non-blocking check if the child has exited
        match child.try_wait() {
            Ok(Some(status)) => {
                let exit_code = status.code().unwrap_or(-1);
                warn!(
                    name = %proc.name,
                    exit_code,
                    "Process exited"
                );

                // Clean up PID file
                let _ = std::fs::remove_file(paths.plugin_pid_file(&proc.name));
                proc.child = None;

                // Check for stopped marker (manual stop)
                if paths.plugin_stopped_file(&proc.name).exists() {
                    info!(name = %proc.name, "Process was manually stopped, not restarting");
                    continue;
                }

                // Restart with backoff
                if proc.should_restart() {
                    let delay = proc.backoff_delay();
                    warn!(
                        name = %proc.name,
                        restart_count = proc.restart_count,
                        delay_secs = delay.as_secs(),
                        "Restarting after backoff"
                    );
                    tokio::time::sleep(delay).await;

                    if let Err(e) = proc.spawn(paths) {
                        error!(name = %proc.name, error = %e, "Failed to restart");
                    }
                } else {
                    error!(
                        name = %proc.name,
                        "Process crashed {} times in {} seconds, not restarting",
                        MAX_RESTARTS,
                        BACKOFF_WINDOW.as_secs()
                    );
                }
            }
            Ok(None) => {
                // Still running
            }
            Err(e) => {
                warn!(name = %proc.name, error = %e, "Error checking process status");
            }
        }
    }
}

/// Gracefully shut down all processes: SIGTERM plugins first, then kernel.
async fn shutdown(processes: &mut Vec<ManagedProcess>, paths: &SeidrumPaths) {
    info!("Shutting down all processes...");

    // Send SIGTERM to plugins (not kernel)
    for proc in processes.iter_mut().filter(|p| p.name != "kernel") {
        if let Some(ref mut child) = proc.child {
            let pid = child.id().unwrap_or(0);
            if pid > 0 {
                info!(name = %proc.name, %pid, "Sending SIGTERM");
                let _ = child.start_kill();
            }
        }
    }

    // Wait for plugins to exit
    tokio::time::sleep(SHUTDOWN_TIMEOUT).await;

    // Now stop kernel
    for proc in processes.iter_mut().filter(|p| p.name == "kernel") {
        if let Some(ref mut child) = proc.child {
            let pid = child.id().unwrap_or(0);
            if pid > 0 {
                info!(name = %proc.name, %pid, "Sending SIGTERM to kernel");
                let _ = child.start_kill();
            }
        }
    }

    // Wait briefly for kernel
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Clean up PID files
    for proc in processes.iter() {
        let _ = std::fs::remove_file(paths.plugin_pid_file(&proc.name));
        let _ = std::fs::remove_file(paths.plugin_meta_file(&proc.name));
    }
}

/// Stop the daemon by reading its PID file and sending SIGTERM.
pub async fn stop(paths: &SeidrumPaths) -> Result<()> {
    let pid_file = paths.daemon_pid_file();
    let pid_str = std::fs::read_to_string(&pid_file)
        .context("Daemon does not appear to be running (no PID file)")?;
    let pid: i32 = pid_str.trim().parse().context("Invalid PID file")?;

    info!(%pid, "Sending SIGTERM to daemon");
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }

    // Wait for the daemon to exit
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if unsafe { libc::kill(pid, 0) } != 0 {
            println!("Daemon stopped");
            return Ok(());
        }
    }

    warn!("Daemon did not stop after 30s, sending SIGKILL");
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }

    Ok(())
}

/// Delegate a subcommand to the seidrum-kernel binary.
pub async fn run_kernel_command(paths: &SeidrumPaths, args: &[&str]) -> Result<()> {
    let kernel_binary = paths.plugin_binary("seidrum-kernel");

    if !kernel_binary.exists() {
        anyhow::bail!("seidrum-kernel not found at {}", kernel_binary.display());
    }

    let status = std::process::Command::new(&kernel_binary)
        .args(args)
        .status()
        .with_context(|| format!("Failed to run seidrum-kernel {}", args.join(" ")))?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}

/// Start a single plugin on a running daemon.
pub async fn start_plugin(paths: &SeidrumPaths, name: &str) -> Result<()> {
    let config = load_plugins_config(&paths.plugins_yaml())?;
    let entry = config
        .plugins
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("Plugin '{}' not found in plugins.yaml", name))?;

    // Remove stopped marker
    let _ = std::fs::remove_file(paths.plugin_stopped_file(name));

    // Check if already running
    if let Ok(pid_str) = std::fs::read_to_string(paths.plugin_pid_file(name)) {
        if let Ok(pid) = pid_str.trim().parse::<i32>() {
            if unsafe { libc::kill(pid, 0) } == 0 {
                println!("Plugin '{}' is already running (PID {})", name, pid);
                return Ok(());
            }
        }
    }

    let env = resolve_plugin_env(entry);
    let mut proc = ManagedProcess::new(name.to_string(), entry.binary.clone(), vec![], env);

    proc.spawn(paths)?;

    // Detach — the monitoring loop in the running daemon will pick it up
    if let Some(mut child) = proc.child.take() {
        // Forget the child so it doesn't get killed when we exit
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
    }

    println!("Plugin '{}' started", name);
    Ok(())
}

/// Stop a single plugin on a running daemon.
pub async fn stop_plugin(paths: &SeidrumPaths, name: &str) -> Result<()> {
    let pid_file = paths.plugin_pid_file(name);
    let pid_str = std::fs::read_to_string(&pid_file)
        .with_context(|| format!("Plugin '{}' does not appear to be running", name))?;
    let pid: i32 = pid_str.trim().parse().context("Invalid PID file")?;

    // Write stopped marker so the daemon doesn't restart it
    std::fs::write(paths.plugin_stopped_file(name), "")?;

    info!(name = %name, %pid, "Sending SIGTERM to plugin");
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }

    // Wait briefly
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if unsafe { libc::kill(pid, 0) } != 0 {
            let _ = std::fs::remove_file(&pid_file);
            println!("Plugin '{}' stopped", name);
            return Ok(());
        }
    }

    warn!("Plugin did not stop after 5s, sending SIGKILL");
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    let _ = std::fs::remove_file(&pid_file);

    println!("Plugin '{}' killed", name);
    Ok(())
}
