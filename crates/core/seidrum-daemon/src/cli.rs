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
    /// Package manager: install, search, publish, and manage packages
    Pkg {
        #[command(subcommand)]
        action: PkgAction,
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

#[derive(Subcommand, Debug)]
pub enum PkgAction {
    /// Search the registry for packages
    Search {
        /// Search query
        query: String,
    },
    /// Install a package from the registry or a URL
    Install {
        /// Package name, name@version, or URL
        package: String,
        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Uninstall a package
    Uninstall {
        /// Package name
        name: String,
        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// List installed packages
    List,
    /// Show package details
    Info {
        /// Package name
        name: String,
    },
    /// Update installed packages
    Update {
        /// Specific package to update (all if omitted)
        name: Option<String>,
    },
    /// Publish current directory as a package to the registry
    Publish {
        /// Registry to publish to
        #[arg(long, default_value = "default")]
        registry: String,
    },
    /// Manage registries
    Registry {
        #[command(subcommand)]
        action: RegistryAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum RegistryAction {
    /// Add a custom registry
    Add {
        /// Registry name
        name: String,
        /// Git URL of the registry
        url: String,
    },
    /// List configured registries
    List,
    /// Remove a custom registry
    Remove {
        /// Registry name
        name: String,
    },
    /// Sync registry index
    Sync {
        /// Registry name (all if omitted)
        name: Option<String>,
    },
}
