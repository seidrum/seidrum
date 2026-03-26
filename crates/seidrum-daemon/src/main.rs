mod cli;
mod config;
mod daemon;
pub mod infra;
mod install;
mod paths;
mod setup;
mod status;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Commands, PluginAction, ServiceAction};
use paths::SeidrumPaths;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let paths = SeidrumPaths::resolve(&cli.config_dir);

    match cli.command {
        Commands::Setup { defaults } => setup::run(&paths, defaults).await,
        Commands::Start => {
            // Start managed infra if configured, otherwise assume external infra
            let infra = infra::InfraManager::load(&paths)?;
            if let Some(mgr) = &infra {
                paths.ensure_dirs()?;
                println!("Starting infrastructure...");
                mgr.start_nats()?;
                mgr.start_arango()?;
                mgr.wait_for_healthy().await?;
                println!("Infrastructure ready.");
            } else if !infra::is_nats_reachable(4222) {
                println!("No managed infrastructure and NATS is not reachable on :4222.");
                println!("Run 'seidrum setup' to configure, or start NATS/ArangoDB manually.");
                return Ok(());
            }
            daemon::start(&paths).await
        }
        Commands::Stop => {
            // Stop daemon first, then infrastructure
            let _ = daemon::stop(&paths).await;

            if let Ok(Some(mgr)) = infra::InfraManager::load(&paths) {
                println!("Stopping infrastructure...");
                mgr.stop_nats()?;
                mgr.stop_arango()?;
                println!("Infrastructure stopped.");
            }
            Ok(())
        }
        Commands::Status => {
            // Show infra status, then daemon/plugin status
            if let Ok(Some(mgr)) = infra::InfraManager::load(&paths) {
                println!("Infrastructure:");
                let nats_status = if mgr.is_nats_running() {
                    match mgr.nats_pid() {
                        Some(pid) => {
                            format!("running (PID {}, port {})", pid, mgr.config.nats.port)
                        }
                        None => format!("running (port {})", mgr.config.nats.port),
                    }
                } else {
                    "not running".to_string()
                };
                let arango_status = if mgr.is_arango_running() {
                    format!(
                        "running (container {}, port {})",
                        mgr.arango_container_name(),
                        mgr.config.arango.port
                    )
                } else {
                    "not running".to_string()
                };
                println!("  NATS:     {}", nats_status);
                println!("  ArangoDB: {}", arango_status);
                println!();
            }
            status::show(&paths).await
        }
        Commands::Plugin { action } => match action {
            PluginAction::List => config::list_plugins(&paths),
            PluginAction::Enable { name } => config::set_enabled(&paths, &name, true),
            PluginAction::Disable { name } => config::set_enabled(&paths, &name, false),
            PluginAction::Start { name } => daemon::start_plugin(&paths, &name).await,
            PluginAction::Stop { name } => daemon::stop_plugin(&paths, &name).await,
            PluginAction::Restart { name } => {
                let _ = daemon::stop_plugin(&paths, &name).await;
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                daemon::start_plugin(&paths, &name).await
            }
        },
        Commands::Service { action } => match action {
            ServiceAction::Install => install::install(&paths),
            ServiceAction::Uninstall => install::uninstall(&paths),
        },
    }
}
