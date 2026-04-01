//! System service installation: systemd (Linux) and launchd (macOS).

use anyhow::{Context, Result};

use crate::paths::SeidrumPaths;

/// Install the daemon as a system service.
pub fn install(paths: &SeidrumPaths) -> Result<()> {
    let seidrum_bin = paths.plugin_binary("seidrum");
    let config_dir =
        std::fs::canonicalize(&paths.config_dir).unwrap_or_else(|_| paths.config_dir.clone());
    let working_dir = std::env::current_dir().unwrap_or_else(|_| ".".into());

    if cfg!(target_os = "linux") {
        install_systemd(&seidrum_bin, &config_dir, &working_dir)
    } else if cfg!(target_os = "macos") {
        install_launchd(&seidrum_bin, &config_dir, &working_dir)
    } else {
        anyhow::bail!("Service installation is not supported on this OS");
    }
}

/// Remove the system service.
pub fn uninstall(_paths: &SeidrumPaths) -> Result<()> {
    if cfg!(target_os = "linux") {
        uninstall_systemd()
    } else if cfg!(target_os = "macos") {
        uninstall_launchd()
    } else {
        anyhow::bail!("Service uninstallation is not supported on this OS");
    }
}

// ---------------------------------------------------------------------------
// systemd (Linux)
// ---------------------------------------------------------------------------

fn systemd_unit_path() -> std::path::PathBuf {
    let uid = unsafe { libc::getuid() };
    if uid == 0 {
        std::path::PathBuf::from("/etc/systemd/system/seidrum.service")
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        std::path::PathBuf::from(format!("{}/.config/systemd/user/seidrum.service", home))
    }
}

fn install_systemd(
    seidrum_bin: &std::path::Path,
    config_dir: &std::path::Path,
    working_dir: &std::path::Path,
) -> Result<()> {
    let unit_path = systemd_unit_path();
    let is_user = unsafe { libc::getuid() } != 0;

    // Ensure parent directory exists
    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let unit = format!(
        r#"[Unit]
Description=Seidrum AI Platform
After=network.target

[Service]
Type=simple
ExecStart={bin} start --config-dir {config}
ExecStop={bin} stop --config-dir {config}
Restart=on-failure
RestartSec=5
WorkingDirectory={workdir}

[Install]
WantedBy={target}
"#,
        bin = seidrum_bin.display(),
        config = config_dir.display(),
        workdir = working_dir.display(),
        target = if is_user {
            "default.target"
        } else {
            "multi-user.target"
        }
    );

    std::fs::write(&unit_path, unit)
        .with_context(|| format!("Failed to write {}", unit_path.display()))?;

    println!("Installed systemd service at {}", unit_path.display());

    // Reload systemd, enable, and start
    let user_args: &[&str] = if is_user { &["--user"] } else { &[] };

    run_systemctl(user_args, "daemon-reload")?;
    run_systemctl(user_args, "enable seidrum")?;
    run_systemctl(user_args, "start seidrum")?;

    println!("Seidrum service installed, enabled, and started.");

    Ok(())
}

fn run_systemctl(user_args: &[&str], command: &str) -> Result<()> {
    let args: Vec<&str> = user_args
        .iter()
        .copied()
        .chain(command.split_whitespace())
        .collect();
    let status = std::process::Command::new("systemctl")
        .args(&args)
        .status()
        .with_context(|| format!("Failed to run: systemctl {}", args.join(" ")))?;
    if !status.success() {
        anyhow::bail!(
            "systemctl {} failed with exit code {:?}",
            args.join(" "),
            status.code()
        );
    }
    Ok(())
}

fn uninstall_systemd() -> Result<()> {
    let unit_path = systemd_unit_path();
    let is_user = unsafe { libc::getuid() } != 0;
    let _user_flag = if is_user { " --user" } else { "" };

    // Stop and disable first
    let _ = std::process::Command::new("systemctl")
        .args(if is_user {
            vec!["--user", "stop", "seidrum"]
        } else {
            vec!["stop", "seidrum"]
        })
        .status();

    let _ = std::process::Command::new("systemctl")
        .args(if is_user {
            vec!["--user", "disable", "seidrum"]
        } else {
            vec!["disable", "seidrum"]
        })
        .status();

    if unit_path.exists() {
        std::fs::remove_file(&unit_path)?;
        let user_args: &[&str] = if is_user { &["--user"] } else { &[] };
        let _ = run_systemctl(user_args, "daemon-reload");
        println!("Removed {}", unit_path.display());
    } else {
        println!("Service file not found at {}", unit_path.display());
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// launchd (macOS)
// ---------------------------------------------------------------------------

fn launchd_plist_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(format!(
        "{}/Library/LaunchAgents/com.seidrum.daemon.plist",
        home
    ))
}

fn install_launchd(
    seidrum_bin: &std::path::Path,
    config_dir: &std::path::Path,
    working_dir: &std::path::Path,
) -> Result<()> {
    let plist_path = launchd_plist_path();

    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.seidrum.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>start</string>
        <string>--config-dir</string>
        <string>{config}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>WorkingDirectory</key>
    <string>{workdir}</string>
</dict>
</plist>
"#,
        bin = seidrum_bin.display(),
        config = config_dir.display(),
        workdir = working_dir.display(),
    );

    std::fs::write(&plist_path, plist)
        .with_context(|| format!("Failed to write {}", plist_path.display()))?;

    println!("Installed launchd service at {}", plist_path.display());

    // Load the service
    let status = std::process::Command::new("launchctl")
        .args(["load", &plist_path.display().to_string()])
        .status()
        .context("Failed to run launchctl load")?;

    if status.success() {
        println!("Seidrum service installed and started.");
    } else {
        println!(
            "Service file installed. Run manually: launchctl load {}",
            plist_path.display()
        );
    }

    Ok(())
}

fn uninstall_launchd() -> Result<()> {
    let plist_path = launchd_plist_path();

    let _ = std::process::Command::new("launchctl")
        .args(["unload", &plist_path.display().to_string()])
        .status();

    if plist_path.exists() {
        std::fs::remove_file(&plist_path)?;
        println!("Removed {}", plist_path.display());
    } else {
        println!("Plist not found at {}", plist_path.display());
    }

    Ok(())
}
