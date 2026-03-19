use clap::Parser;
use tracing::info;

#[derive(Parser)]
#[command(name = "seidrum-cli", about = "Seidrum CLI channel plugin")]
struct Cli {
    /// NATS server URL
    #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4222")]
    nats_url: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    info!(nats_url = %cli.nats_url, "Starting seidrum-cli plugin...");

    Ok(())
}
