use clap::{Parser, Subcommand};
use seidrum_common::config::{load_agent_config, load_platform_config, AgentConfigFile};
use std::path::Path;
use std::process;
use tracing::{error, info, warn};

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
    Validate {
        /// Path to platform config file
        #[arg(long, default_value = "config/platform.yaml")]
        config: String,
    },
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
            run_init().await?;
        }
        Command::Validate { config } => {
            info!("Validating configuration...");
            if !run_validate(&config) {
                process::exit(1);
            }
        }
    }

    Ok(())
}

/// Run brain initialization: connect to ArangoDB, create collections, graph,
/// indexes, views, and seed the root scope.
async fn run_init() -> anyhow::Result<()> {
    // 1. Resolve ArangoDB connection parameters from config or env vars.
    let config_path = Path::new("config/platform.yaml");
    let platform_config = load_platform_config(config_path).ok();

    let arango_url = std::env::var("ARANGO_URL")
        .ok()
        .or_else(|| platform_config.as_ref().map(|c| c.arango_url.clone()))
        .unwrap_or_else(|| "http://localhost:8529".to_string());

    let arango_database = std::env::var("ARANGO_DATABASE")
        .ok()
        .or_else(|| platform_config.as_ref().map(|c| c.arango_database.clone()))
        .unwrap_or_else(|| "seidrum".to_string());

    let arango_password = std::env::var("ARANGO_PASSWORD").unwrap_or_default();

    if platform_config.is_none() {
        warn!(
            "Platform config not found at {}; using env vars / defaults",
            config_path.display()
        );
    }

    info!("ArangoDB URL: {}", arango_url);
    info!("ArangoDB database: {}", arango_database);

    // 2. Build the ArangoDB HTTP client.
    let client =
        brain::client::ArangoClient::new(&arango_url, &arango_database, &arango_password)?;

    // 3. Run initialization.
    brain::init::initialize_brain(&client).await?;

    Ok(())
}

/// Run full validation of platform config and all agent definitions.
/// Returns true if everything is valid, false otherwise.
fn run_validate(config_path: &str) -> bool {
    let mut all_ok = true;

    // 1. Parse platform config
    let platform_path = Path::new(config_path);
    let platform_config = match load_platform_config(platform_path) {
        Ok(cfg) => {
            info!("[OK] Platform config: {}", platform_path.display());
            Some(cfg)
        }
        Err(e) => {
            error!("[FAIL] Platform config: {}", e);
            all_ok = false;
            None
        }
    };

    // 2. Determine agents directory
    let agents_dir = platform_config
        .as_ref()
        .map(|c| c.agents_dir.as_str())
        .unwrap_or("agents/");
    let agents_path = Path::new(agents_dir);

    if !agents_path.is_dir() {
        error!("[FAIL] Agents directory not found: {}", agents_path.display());
        return false;
    }

    // 3. Parse all agent YAML files
    let entries = match std::fs::read_dir(agents_path) {
        Ok(entries) => entries,
        Err(e) => {
            error!("[FAIL] Cannot read agents directory: {}", e);
            return false;
        }
    };

    let mut agent_configs: Vec<(String, AgentConfigFile)> = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                error!("[FAIL] Error reading directory entry: {}", e);
                all_ok = false;
                continue;
            }
        };

        let path = entry.path();

        // Only process .yaml and .yml files
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "yaml" && ext != "yml" {
            continue;
        }

        match load_agent_config(&path) {
            Ok(agent_file) => {
                info!(
                    "[OK] Agent config: {} (id: {})",
                    path.display(),
                    agent_file.agent.id
                );
                agent_configs.push((path.display().to_string(), agent_file));
            }
            Err(e) => {
                error!("[FAIL] Agent config: {}", e);
                all_ok = false;
            }
        }
    }

    // 4. Check that prompt files referenced by agents exist
    for (source_path, agent_file) in &agent_configs {
        let agent = &agent_file.agent;

        // Check pipeline steps for prompt references
        for step in &agent.pipeline.steps {
            if let Some(config) = &step.config {
                if let Some(prompt_val) = config.get("prompt") {
                    if let Some(prompt_path) = prompt_val.as_str() {
                        let prompt_file = Path::new(prompt_path);
                        if prompt_file.exists() {
                            info!(
                                "[OK] Prompt file: {} (referenced by agent '{}', step '{}')",
                                prompt_path, agent.id, step.plugin
                            );
                        } else {
                            error!(
                                "[FAIL] Prompt file not found: {} (referenced by agent '{}' in {}, step '{}')",
                                prompt_path, agent.id, source_path, step.plugin
                            );
                            all_ok = false;
                        }
                    }
                }
            }
        }

        // Check background steps for prompt references
        if let Some(bg_steps) = &agent.background {
            for step in bg_steps {
                if let Some(config) = &step.config {
                    if let Some(prompt_val) = config.get("prompt") {
                        if let Some(prompt_path) = prompt_val.as_str() {
                            let prompt_file = Path::new(prompt_path);
                            if prompt_file.exists() {
                                info!(
                                    "[OK] Prompt file: {} (referenced by agent '{}', background step '{}')",
                                    prompt_path, agent.id, step.plugin
                                );
                            } else {
                                error!(
                                    "[FAIL] Prompt file not found: {} (referenced by agent '{}' in {}, background step '{}')",
                                    prompt_path, agent.id, source_path, step.plugin
                                );
                                all_ok = false;
                            }
                        }
                    }
                }
            }
        }
    }

    // 5. Summary
    if all_ok {
        info!(
            "Validation complete: all OK ({} agent(s) validated)",
            agent_configs.len()
        );
    } else {
        error!("Validation complete: errors found. See above for details.");
    }

    all_ok
}
