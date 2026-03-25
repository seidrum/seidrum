mod cli;
mod config;
mod daemon;
mod install;
mod paths;
mod status;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Commands, DaemonAction, PluginAction, SkillAction};
use paths::SeidrumPaths;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let paths = SeidrumPaths::resolve(&cli.config_dir);

    match cli.command {
        Commands::Daemon { action } => match action {
            DaemonAction::Start => daemon::start(&paths).await,
            DaemonAction::Stop => daemon::stop(&paths).await,
            DaemonAction::Restart => {
                let _ = daemon::stop(&paths).await;
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                daemon::start(&paths).await
            }
            DaemonAction::Status => status::show(&paths).await,
            DaemonAction::Install => install::install(&paths),
            DaemonAction::Uninstall => install::uninstall(&paths),
        },
        Commands::Init => daemon::run_kernel_command(&paths, &["init"]).await,
        Commands::Validate { config } => {
            daemon::run_kernel_command(&paths, &["validate", "--config", &config]).await
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
        Commands::Skill { action } => match action {
            SkillAction::List => {
                println!("Skills are stored in ArangoDB. Use the dashboard or agent capabilities to manage skills.");
                println!("System skills are loaded from the skills/ directory on kernel startup.");
                Ok(())
            }
            SkillAction::Install { path } => {
                println!("Installing skill from {}...", path);
                println!("Note: The kernel must be running to install skills.");
                println!("Copy the file to skills/ and restart the kernel, or use the dashboard.");
                Ok(())
            }
            SkillAction::Remove { skill_id } => {
                println!("Removing skill '{}'...", skill_id);
                println!("Note: Use the dashboard to remove skills from the database.");
                Ok(())
            }
        },
    }
}
