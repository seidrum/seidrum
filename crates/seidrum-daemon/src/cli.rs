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
    /// First-run setup wizard (downloads NATS, configures ArangoDB, generates .env)
    Setup {
        /// Skip interactive prompts, use generated defaults
        #[arg(long)]
        defaults: bool,
    },
    /// Start everything (infrastructure + kernel + plugins)
    Start,
    /// Stop everything (plugins + kernel + infrastructure)
    Stop,
    /// Show status of infrastructure and all processes
    Status,
    /// Manage plugins
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },
    /// Install or remove the system service (systemd/launchd)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand)]
pub enum ServiceAction {
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
