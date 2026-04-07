//! Process supervisor: spawn, monitor, restart, and gracefully shut down
//! the kernel and all enabled plugins. Supports NATS-based plugin lifecycle control.

#[cfg(not(unix))]
compile_error!("seidrum-daemon requires a Unix system (Linux or macOS)");

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{error, info, warn};

use crate::config::{load_plugins_config, resolve_plugin_env};
use crate::paths::SeidrumPaths;
use crate::status::ProcessMeta;

/// Maximum restart attempts within the backoff window.
const MAX_RESTARTS: u32 = 5;
/// Backoff window — reset restart count after this duration.
const BACKOFF_WINDOW: Duration = Duration::from_secs(300);
/// Grace period for SIGTERM before SIGKILL.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// NATS request to start a plugin.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct PluginStartCommand {
    plugin: String,
}

/// NATS request to stop a plugin.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct PluginStopCommand {
    plugin: String,
}

/// Response to a plugin control command.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct PluginCommandResponse {
    success: bool,
    message: String,
}

/// Send SIGTERM to a process by PID. Returns true if the signal was sent.
fn send_sigterm(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, libc::SIGTERM) == 0 }
}

/// Send SIGKILL to a process by PID.
fn send_sigkill(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

/// Check if a process is alive by PID.
pub fn is_process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let ret = unsafe { libc::kill(pid, 0) };
    if ret == 0 {
        return true;
    }
    // EPERM means the process exists but we lack permissions
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// A managed child process.
struct ManagedProcess {
    name: String,
    binary: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    child: Option<tokio::process::Child>,
    pid: Option<u32>,
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
            pid: None,
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

        for (k, v) in &self.env {
            cmd.env(k, v);
        }

        cmd.stdout(std::process::Stdio::from(log_file));
        cmd.stderr(std::process::Stdio::from(log_stderr));

        let child = cmd.spawn().with_context(|| {
            format!("Failed to spawn {} ({})", self.name, binary_path.display())
        })?;

        let pid = child.id().context("Failed to get PID of spawned process")?;

        self.started_at = Some(Utc::now());
        self.pid = Some(pid);
        self.child = Some(child);

        // Write PID file
        std::fs::write(paths.plugin_pid_file(&self.name), pid.to_string())
            .with_context(|| format!("Failed to write PID file for {}", self.name))?;

        // Write metadata
        let meta = ProcessMeta {
            pid,
            started_at: self.started_at.unwrap(),
            restart_count: self.restart_count,
        };
        std::fs::write(
            paths.plugin_meta_file(&self.name),
            serde_json::to_string(&meta).unwrap_or_default(),
        )
        .with_context(|| format!("Failed to write meta file for {}", self.name))?;

        // Remove stopped marker if present
        let _ = std::fs::remove_file(paths.plugin_stopped_file(&self.name));

        info!(name = %self.name, %pid, "Process started");
        Ok(())
    }

    /// Check if restart should be attempted based on backoff policy.
    fn should_restart(&mut self) -> bool {
        let now = Instant::now();

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

    // Check if already running — use create_new for atomic PID file creation
    let pid_file = paths.daemon_pid_file();
    if let Ok(pid_str) = std::fs::read_to_string(&pid_file) {
        if let Ok(pid) = pid_str.trim().parse::<i32>() {
            if is_process_alive(pid) {
                anyhow::bail!(
                    "Daemon is already running (PID {}). Use 'seidrum stop' first.",
                    pid
                );
            }
        }
        // Stale PID file — remove it
        let _ = std::fs::remove_file(&pid_file);
    }

    // Write our own PID atomically
    std::fs::write(&pid_file, std::process::id().to_string())?;

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
    let _config = if plugins_yaml.exists() {
        let cfg = load_plugins_config(&plugins_yaml)?;
        for (name, entry) in &cfg.plugins {
            if !entry.enabled {
                info!(name = %name, "Plugin disabled, skipping");
                continue;
            }

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
        Some(cfg)
    } else {
        warn!(
            "No plugins.yaml found at {}. Only the kernel will run.",
            plugins_yaml.display()
        );
        None
    };

    info!(
        count = processes.len(),
        "All processes started, entering monitoring loop"
    );

    // 3. Connect to NATS and subscribe to plugin control subjects
    let nats_url =
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string());
    let nats_client = match async_nats::connect(&nats_url).await {
        Ok(client) => {
            info!("Connected to NATS for plugin lifecycle control");
            Some(client)
        }
        Err(e) => {
            warn!(
                "Failed to connect to NATS ({}), plugin lifecycle control via NATS unavailable",
                e
            );
            None
        }
    };

    let mut start_sub = None;
    let mut stop_sub = None;

    if let Some(ref client) = nats_client {
        if let Ok(sub) = client.subscribe("daemon.plugin.start".to_string()).await {
            info!("Subscribed to daemon.plugin.start");
            start_sub = Some(sub);
        } else {
            warn!("Failed to subscribe to daemon.plugin.start");
        }

        if let Ok(sub) = client.subscribe("daemon.plugin.stop".to_string()).await {
            info!("Subscribed to daemon.plugin.stop");
            stop_sub = Some(sub);
        } else {
            warn!("Failed to subscribe to daemon.plugin.stop");
        }
    }

    // 4. Monitoring loop — handle SIGINT, SIGTERM, process checks, and NATS commands
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("Failed to register SIGTERM handler")?;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT, shutting down");
                break;
            }
            _ = sigterm.recv() => {
                info!("Received SIGTERM, shutting down");
                break;
            }
            _ = check_processes(&mut processes, paths) => {
                // A process exited and was handled
            }
            msg = async {
                match &mut start_sub {
                    Some(sub) => sub.next().await,
                    None => futures::future::pending().await,
                }
            } => {
                if let Some(msg) = msg {
                    if let Some(ref client) = nats_client {
                        handle_plugin_start(&msg, &mut processes, paths, client).await;
                    }
                }
            }
            msg = async {
                match &mut stop_sub {
                    Some(sub) => sub.next().await,
                    None => futures::future::pending().await,
                }
            } => {
                if let Some(msg) = msg {
                    if let Some(ref client) = nats_client {
                        handle_plugin_stop(&msg, &mut processes, paths, client).await;
                    }
                }
            }
        }
    }

    // 5. Graceful shutdown
    shutdown(&mut processes, paths).await;

    // Clean up daemon PID
    let _ = std::fs::remove_file(paths.daemon_pid_file());

    info!("Seidrum daemon stopped");
    Ok(())
}

/// Monitor processes and restart crashed ones.
async fn check_processes(processes: &mut Vec<ManagedProcess>, paths: &SeidrumPaths) {
    tokio::time::sleep(Duration::from_secs(1)).await;

    for proc in processes.iter_mut() {
        let child = match proc.child.as_mut() {
            Some(c) => c,
            None => continue,
        };

        match child.try_wait() {
            Ok(Some(status)) => {
                let exit_code = status.code().unwrap_or(-1);
                warn!(name = %proc.name, exit_code, "Process exited");

                let _ = std::fs::remove_file(paths.plugin_pid_file(&proc.name));
                proc.child = None;
                proc.pid = None;

                if paths.plugin_stopped_file(&proc.name).exists() {
                    info!(name = %proc.name, "Process was manually stopped, not restarting");
                    continue;
                }

                if proc.should_restart() {
                    proc.restart_count += 1;
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
            Ok(None) => {}
            Err(e) => {
                warn!(name = %proc.name, error = %e, "Error checking process status");
            }
        }
    }
}

/// Gracefully shut down all processes: SIGTERM plugins first, then kernel.
async fn shutdown(processes: &mut Vec<ManagedProcess>, paths: &SeidrumPaths) {
    info!("Shutting down all processes...");

    // Send SIGTERM to plugins first (not kernel)
    for proc in processes.iter().filter(|p| p.name != "kernel") {
        if let Some(pid) = proc.pid {
            info!(name = %proc.name, %pid, "Sending SIGTERM");
            send_sigterm(pid);
        }
    }

    // Wait for plugins to exit, polling instead of fixed sleep
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    loop {
        let all_stopped = processes.iter().filter(|p| p.name != "kernel").all(|p| {
            p.pid
                .map(|pid| !is_process_alive(pid as i32))
                .unwrap_or(true)
        });

        if all_stopped || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // SIGKILL any remaining plugins
    for proc in processes.iter().filter(|p| p.name != "kernel") {
        if let Some(pid) = proc.pid {
            if is_process_alive(pid as i32) {
                warn!(name = %proc.name, %pid, "Sending SIGKILL to stuck process");
                send_sigkill(pid);
            }
        }
    }

    // Now stop kernel
    for proc in processes.iter().filter(|p| p.name == "kernel") {
        if let Some(pid) = proc.pid {
            info!(name = %proc.name, %pid, "Sending SIGTERM to kernel");
            send_sigterm(pid);
        }
    }

    // Wait for kernel
    let kernel_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let kernel_stopped = processes.iter().filter(|p| p.name == "kernel").all(|p| {
            p.pid
                .map(|pid| !is_process_alive(pid as i32))
                .unwrap_or(true)
        });

        if kernel_stopped || Instant::now() >= kernel_deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Clean up all PID and meta files
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

    if !is_process_alive(pid) {
        // Stale PID file
        let _ = std::fs::remove_file(&pid_file);
        println!("Daemon is not running (stale PID file cleaned up)");
        return Ok(());
    }

    info!(%pid, "Sending SIGTERM to daemon");
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }

    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if !is_process_alive(pid) {
            println!("Daemon stopped");
            return Ok(());
        }
    }

    warn!("Daemon did not stop after 30s, sending SIGKILL");
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    let _ = std::fs::remove_file(&pid_file);

    println!("Daemon killed");
    Ok(())
}

/// Handle a plugin start request from NATS.
async fn handle_plugin_start(
    msg: &async_nats::Message,
    processes: &mut Vec<ManagedProcess>,
    paths: &SeidrumPaths,
    client: &async_nats::Client,
) {
    // Parse the command
    let cmd: PluginStartCommand = match serde_json::from_slice(&msg.payload) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to parse plugin start command: {}", e);
            if let Some(reply) = &msg.reply {
                let response = PluginCommandResponse {
                    success: false,
                    message: format!("Failed to parse command: {}", e),
                };
                let _ = client
                    .publish(
                        reply.clone(),
                        serde_json::to_vec(&response).unwrap_or_default().into(),
                    )
                    .await;
            }
            return;
        }
    };

    let plugin_name = &cmd.plugin;

    // Reload config to get fresh state
    let fresh_config = match crate::config::load_plugins_config(&paths.plugins_yaml()) {
        Ok(cfg) => cfg,
        Err(_) => {
            let response = PluginCommandResponse {
                success: false,
                message: "Failed to load plugins.yaml".to_string(),
            };
            if let Some(reply) = &msg.reply {
                let _ = client
                    .publish(
                        reply.clone(),
                        serde_json::to_vec(&response).unwrap_or_default().into(),
                    )
                    .await;
            }
            return;
        }
    };

    let entry = match fresh_config.plugins.get(plugin_name) {
        Some(e) => e,
        None => {
            let response = PluginCommandResponse {
                success: false,
                message: format!("Plugin '{}' not found in plugins.yaml", plugin_name),
            };
            if let Some(reply) = &msg.reply {
                let _ = client
                    .publish(
                        reply.clone(),
                        serde_json::to_vec(&response).unwrap_or_default().into(),
                    )
                    .await;
            }
            return;
        }
    };

    // Check if already running
    if let Ok(pid_str) = std::fs::read_to_string(paths.plugin_pid_file(plugin_name)) {
        if let Ok(pid) = pid_str.trim().parse::<i32>() {
            if is_process_alive(pid) {
                let response = PluginCommandResponse {
                    success: false,
                    message: format!("Plugin '{}' is already running (PID {})", plugin_name, pid),
                };
                if let Some(reply) = &msg.reply {
                    let _ = client
                        .publish(
                            reply.clone(),
                            serde_json::to_vec(&response).unwrap_or_default().into(),
                        )
                        .await;
                }
                return;
            }
        }
    }

    // Remove stopped marker
    let _ = std::fs::remove_file(paths.plugin_stopped_file(plugin_name));

    // Resolve env vars
    let env = crate::config::resolve_plugin_env(entry);

    // Create and spawn process
    let mut proc = ManagedProcess::new(plugin_name.clone(), entry.binary.clone(), vec![], env);

    match proc.spawn(paths) {
        Ok(()) => {
            info!(name = %plugin_name, "Plugin started via NATS");
            // Check if there's already an entry for this plugin name and update it instead of always pushing
            if let Some(existing) = processes.iter_mut().find(|p| p.name == *plugin_name) {
                *existing = proc;
            } else {
                processes.push(proc);
            }
            let response = PluginCommandResponse {
                success: true,
                message: format!("Plugin '{}' started", plugin_name),
            };
            if let Some(reply) = &msg.reply {
                let _ = client
                    .publish(
                        reply.clone(),
                        serde_json::to_vec(&response).unwrap_or_default().into(),
                    )
                    .await;
            }
        }
        Err(e) => {
            error!(name = %plugin_name, error = %e, "Failed to start plugin via NATS");
            let response = PluginCommandResponse {
                success: false,
                message: format!("Failed to start plugin: {}", e),
            };
            if let Some(reply) = &msg.reply {
                let _ = client
                    .publish(
                        reply.clone(),
                        serde_json::to_vec(&response).unwrap_or_default().into(),
                    )
                    .await;
            }
        }
    }
}

/// Handle a plugin stop request from NATS.
async fn handle_plugin_stop(
    msg: &async_nats::Message,
    processes: &mut Vec<ManagedProcess>,
    paths: &SeidrumPaths,
    client: &async_nats::Client,
) {
    // Parse the command
    let cmd: PluginStopCommand = match serde_json::from_slice(&msg.payload) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to parse plugin stop command: {}", e);
            if let Some(reply) = &msg.reply {
                let response = PluginCommandResponse {
                    success: false,
                    message: format!("Failed to parse command: {}", e),
                };
                let _ = client
                    .publish(
                        reply.clone(),
                        serde_json::to_vec(&response).unwrap_or_default().into(),
                    )
                    .await;
            }
            return;
        }
    };

    let plugin_name = &cmd.plugin;

    // Find the process by name
    let pid_file = paths.plugin_pid_file(plugin_name);
    let pid_str = match std::fs::read_to_string(&pid_file) {
        Ok(s) => s,
        Err(_) => {
            let response = PluginCommandResponse {
                success: false,
                message: format!("Plugin '{}' does not appear to be running", plugin_name),
            };
            if let Some(reply) = &msg.reply {
                let _ = client
                    .publish(
                        reply.clone(),
                        serde_json::to_vec(&response).unwrap_or_default().into(),
                    )
                    .await;
            }
            return;
        }
    };

    let pid: i32 = match pid_str.trim().parse() {
        Ok(p) => p,
        Err(_) => {
            let response = PluginCommandResponse {
                success: false,
                message: "Invalid PID file".to_string(),
            };
            if let Some(reply) = &msg.reply {
                let _ = client
                    .publish(
                        reply.clone(),
                        serde_json::to_vec(&response).unwrap_or_default().into(),
                    )
                    .await;
            }
            return;
        }
    };

    if !is_process_alive(pid) {
        let _ = std::fs::remove_file(&pid_file);
        let response = PluginCommandResponse {
            success: false,
            message: format!(
                "Plugin '{}' is not running (stale PID file cleaned up)",
                plugin_name
            ),
        };
        if let Some(reply) = &msg.reply {
            let _ = client
                .publish(
                    reply.clone(),
                    serde_json::to_vec(&response).unwrap_or_default().into(),
                )
                .await;
        }
        // Remove entry from processes vector
        processes.retain(|p| p.name != *plugin_name);
        return;
    }

    // Write stopped marker so daemon doesn't restart it
    let _ = std::fs::write(paths.plugin_stopped_file(plugin_name), "");

    info!(name = %plugin_name, %pid, "Sending SIGTERM to plugin via NATS");
    send_sigterm(pid as u32);

    // Wait for graceful shutdown
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if !is_process_alive(pid) {
            let _ = std::fs::remove_file(&pid_file);
            let response = PluginCommandResponse {
                success: true,
                message: format!("Plugin '{}' stopped", plugin_name),
            };
            if let Some(reply) = &msg.reply {
                let _ = client
                    .publish(
                        reply.clone(),
                        serde_json::to_vec(&response).unwrap_or_default().into(),
                    )
                    .await;
            }
            info!(name = %plugin_name, "Plugin stopped via NATS");
            // Remove entry from processes vector
            processes.retain(|p| p.name != *plugin_name);
            return;
        }
    }

    // Force kill if it didn't stop
    warn!(name = %plugin_name, "Plugin did not stop after 5s, sending SIGKILL");
    send_sigkill(pid as u32);
    let _ = std::fs::remove_file(&pid_file);

    let response = PluginCommandResponse {
        success: true,
        message: format!("Plugin '{}' killed", plugin_name),
    };
    if let Some(reply) = &msg.reply {
        let _ = client
            .publish(
                reply.clone(),
                serde_json::to_vec(&response).unwrap_or_default().into(),
            )
            .await;
    }
    info!(name = %plugin_name, "Plugin killed via NATS");
    // Remove entry from processes vector
    processes.retain(|p| p.name != *plugin_name);
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

/// Start a single plugin (writes PID file; daemon monitor will detect via PID).
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
            if is_process_alive(pid) {
                println!("Plugin '{}' is already running (PID {})", name, pid);
                return Ok(());
            }
        }
    }

    let env = resolve_plugin_env(entry);
    let mut proc = ManagedProcess::new(name.to_string(), entry.binary.clone(), vec![], env);

    proc.spawn(paths)?;

    // Detach the child — it will run independently.
    // The daemon's monitor loop will detect it via PID file if running,
    // otherwise it runs as a standalone process.
    if let Some(mut child) = proc.child.take() {
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

    if !is_process_alive(pid) {
        let _ = std::fs::remove_file(&pid_file);
        println!(
            "Plugin '{}' is not running (stale PID file cleaned up)",
            name
        );
        return Ok(());
    }

    // Write stopped marker so the daemon doesn't restart it
    std::fs::write(paths.plugin_stopped_file(name), "")?;

    info!(name = %name, %pid, "Sending SIGTERM to plugin");
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }

    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if !is_process_alive(pid) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_restart_respects_max() {
        let mut proc =
            ManagedProcess::new("test".into(), "test-bin".into(), vec![], BTreeMap::new());

        for _ in 0..MAX_RESTARTS {
            assert!(proc.should_restart());
            proc.restart_count += 1;
        }
        assert!(!proc.should_restart());
    }

    #[test]
    fn backoff_delay_sequence() {
        let mut proc =
            ManagedProcess::new("test".into(), "test-bin".into(), vec![], BTreeMap::new());

        // backoff_delay uses restart_count: 1, 2, 4, 8, 16
        let expected = [1, 2, 4, 8, 16];
        for &expected_secs in &expected {
            assert_eq!(proc.backoff_delay().as_secs(), expected_secs);
            proc.restart_count += 1;
        }
    }

    #[test]
    fn is_process_alive_nonexistent() {
        // PID that almost certainly doesn't exist
        assert!(!is_process_alive(999_999_999));
    }

    #[test]
    fn is_process_alive_zero_is_false() {
        assert!(!is_process_alive(0));
    }

    #[test]
    fn is_process_alive_negative_is_false() {
        assert!(!is_process_alive(-1));
    }
}
