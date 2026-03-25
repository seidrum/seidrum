//! CLI subcommand definitions.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "seidrum", about = "Seidrum platform manager", version)]
pub struct Cli {
    /// Path to config directory
    #[arg(long, default_value = "config", global = true)]
    pub config_dir: PathBuf,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Manage the Seidrum daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Initialize the brain database
    Init,
    /// Validate configuration
    Validate {
        /// Path to platform config file
        #[arg(long, default_value = "config/platform.yaml")]
        config: String,
    },
    /// Manage plugins
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },
    /// Manage agent skills
    Skill {
        #[command(subcommand)]
        action: SkillAction,
    },
}

#[derive(Subcommand)]
pub enum DaemonAction {
    /// Start kernel + enabled plugins (foreground)
    Start,
    /// Stop the running daemon
    Stop,
    /// Restart the daemon
    Restart,
    /// Show status of all processes
    Status,
    /// Install as a system service (systemd/launchd)
    Install,
    /// Remove the system service
    Uninstall,
}

#[derive(Subcommand)]
pub enum PluginAction {
    /// List all plugins with enabled/disabled state
    List,
    /// Enable a plugin
    Enable {
        /// Plugin name (e.g., "telegram")
        name: String,
    },
    /// Disable a plugin
    Disable {
        /// Plugin name (e.g., "telegram")
        name: String,
    },
    /// Start a specific plugin
    Start {
        /// Plugin name
        name: String,
    },
    /// Stop a specific plugin
    Stop {
        /// Plugin name
        name: String,
    },
    /// Restart a specific plugin
    Restart {
        /// Plugin name
        name: String,
    },
}

#[derive(Subcommand)]
pub enum SkillAction {
    /// List all installed skills
    List,
    /// Install a skill from a YAML file
    Install {
        /// Path to skill YAML file
        path: String,
    },
    /// Remove a skill by ID
    Remove {
        /// Skill ID
        skill_id: String,
    },
}
