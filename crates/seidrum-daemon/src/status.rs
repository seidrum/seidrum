//! Status display: reads PID files and metadata to show process state.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tabled::{Table, Tabled};

use crate::config::load_plugins_config;
use crate::paths::SeidrumPaths;

/// Metadata stored alongside PID files.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProcessMeta {
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub restart_count: u32,
}

/// Show the status of the daemon and all plugins.
pub async fn show(paths: &SeidrumPaths) -> Result<()> {
    // Check daemon
    let daemon_status = check_pid_file(&paths.daemon_pid_file());
    match &daemon_status {
        Some(pid) => println!("Seidrum daemon: running (PID {})\n", pid),
        None => {
            println!("Seidrum daemon: not running\n");
        }
    }

    // Check kernel
    let kernel_status = get_process_status(paths, "kernel");

    // Check plugins
    let plugins_yaml = paths.plugins_yaml();
    let plugin_statuses: Vec<ProcessStatus> = if plugins_yaml.exists() {
        let config = load_plugins_config(&plugins_yaml)?;
        let mut statuses = vec![kernel_status];

        for (name, entry) in &config.plugins {
            let status = if !entry.enabled {
                ProcessStatus {
                    name: name.clone(),
                    binary: entry.binary.clone(),
                    state: "disabled".to_string(),
                    pid: "-".to_string(),
                    uptime: "-".to_string(),
                    restarts: "-".to_string(),
                }
            } else if paths.plugin_stopped_file(name).exists() {
                ProcessStatus {
                    name: name.clone(),
                    binary: entry.binary.clone(),
                    state: "stopped".to_string(),
                    pid: "-".to_string(),
                    uptime: "-".to_string(),
                    restarts: "-".to_string(),
                }
            } else {
                get_process_status(paths, name)
            };
            statuses.push(status);
        }
        statuses
    } else {
        vec![kernel_status]
    };

    println!("{}", Table::new(&plugin_statuses));

    Ok(())
}

#[derive(Tabled)]
struct ProcessStatus {
    #[tabled(rename = "Name")]
    name: String,
    #[tabled(rename = "Binary")]
    binary: String,
    #[tabled(rename = "Status")]
    state: String,
    #[tabled(rename = "PID")]
    pid: String,
    #[tabled(rename = "Uptime")]
    uptime: String,
    #[tabled(rename = "Restarts")]
    restarts: String,
}

fn get_process_status(paths: &SeidrumPaths, name: &str) -> ProcessStatus {
    let binary = if name == "kernel" {
        "seidrum-kernel".to_string()
    } else {
        format!("seidrum-{}", name)
    };

    let pid = check_pid_file(&paths.plugin_pid_file(name));
    let meta = load_meta(&paths.plugin_meta_file(name));

    match pid {
        Some(pid) => {
            let (uptime, restarts) = match meta {
                Some(m) => {
                    let uptime = format_uptime(m.started_at);
                    (uptime, m.restart_count.to_string())
                }
                None => ("-".to_string(), "0".to_string()),
            };

            ProcessStatus {
                name: name.to_string(),
                binary,
                state: "running".to_string(),
                pid: pid.to_string(),
                uptime,
                restarts,
            }
        }
        None => ProcessStatus {
            name: name.to_string(),
            binary,
            state: "down".to_string(),
            pid: "-".to_string(),
            uptime: "-".to_string(),
            restarts: meta
                .map(|m| m.restart_count.to_string())
                .unwrap_or_else(|| "-".to_string()),
        },
    }
}

/// Check if a PID file exists and the process is alive.
fn check_pid_file(path: &std::path::Path) -> Option<i32> {
    let pid_str = std::fs::read_to_string(path).ok()?;
    let pid: i32 = pid_str.trim().parse().ok()?;
    if unsafe { libc::kill(pid, 0) } == 0 {
        Some(pid)
    } else {
        None
    }
}

/// Load metadata from a .meta file.
fn load_meta(path: &std::path::Path) -> Option<ProcessMeta> {
    let contents = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Format uptime as human-readable string.
fn format_uptime(started_at: DateTime<Utc>) -> String {
    let duration = Utc::now() - started_at;
    let total_secs = duration.num_seconds().max(0) as u64;

    if total_secs < 60 {
        format!("{}s", total_secs)
    } else if total_secs < 3600 {
        format!("{}m", total_secs / 60)
    } else if total_secs < 86400 {
        let hours = total_secs / 3600;
        let mins = (total_secs % 3600) / 60;
        format!("{}h {}m", hours, mins)
    } else {
        let days = total_secs / 86400;
        let hours = (total_secs % 86400) / 3600;
        format!("{}d {}h", days, hours)
    }
}
