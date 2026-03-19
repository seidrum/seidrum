use clap::{Parser, Subcommand};
use tracing::info;

mod brain;
mod orchestrator;
mod registry;
mod scheduler;
mod scope;

#[derive(Parser)]
#[command(name = "seidrum-kernel", about = "Seidrum kernel daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the kernel daemon
    Serve,
    /// Initialize the brain database and NATS streams
    Init,
    /// Validate configuration and agent definitions
    Validate,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Command::Serve => {
            info!("Starting seidrum-kernel daemon...");
        }
        Command::Init => {
            info!("Initializing brain database and NATS streams...");
        }
        Command::Validate => {
            info!("Validating configuration...");
        }
    }

    Ok(())
}
