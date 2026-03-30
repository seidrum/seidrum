use clap::{Parser, Subcommand};
use seidrum_common::config::{load_agent_config, load_platform_config, AgentConfigFile};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process;
use tracing::{error, info, warn};

use orchestrator::service::OrchestratorService;
use registry::service::RegistryService;

mod brain;
mod consciousness;
mod embedding;
mod orchestrator;
mod plugin_storage;
mod registry;
mod scheduler;
mod scope;
mod tool_registry;
mod trace_collector;

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
            run_serve().await?;
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

/// Start the kernel daemon: connect to NATS, spawn registry service (and
/// future brain / orchestrator services), then wait for shutdown.
async fn run_serve() -> anyhow::Result<()> {
    // 1. Resolve NATS URL from config or env.
    let config_path = Path::new("config/platform.yaml");
    let platform_config = load_platform_config(config_path).ok();

    let nats_url = std::env::var("NATS_URL")
        .ok()
        .or_else(|| platform_config.as_ref().map(|c| c.nats_url.clone()))
        .unwrap_or_else(|| "nats://localhost:4222".to_string());

    if platform_config.is_none() {
        warn!(
            "Platform config not found at {}; using env vars / defaults",
            config_path.display()
        );
    }

    info!("NATS URL: {}", nats_url);

    // 2. Connect to NATS.
    let nats_client = async_nats::connect(&nats_url)
        .await
        .map_err(|e| anyhow::anyhow!("failed to connect to NATS at {}: {}", nats_url, e))?;
    info!("connected to NATS");

    // 3. Spawn the registry service.
    let registry = RegistryService::new();
    let registry_for_scheduler = registry.clone();
    let registry_handle = registry.spawn(nats_client.clone()).await?;
    info!("registry service started");

    // 4. Build ArangoDB client and spawn the brain service.
    let arango_url = std::env::var("ARANGO_URL")
        .ok()
        .or_else(|| platform_config.as_ref().map(|c| c.arango_url.clone()))
        .unwrap_or_else(|| "http://localhost:8529".to_string());
    let arango_database = std::env::var("ARANGO_DATABASE")
        .ok()
        .or_else(|| platform_config.as_ref().map(|c| c.arango_database.clone()))
        .unwrap_or_else(|| "seidrum".to_string());
    let arango_password = std::env::var("ARANGO_PASSWORD").unwrap_or_default();

    let arango_client =
        brain::client::ArangoClient::new(&arango_url, &arango_database, &arango_password)?;
    info!("ArangoDB client created ({})", arango_url);

    // Create embedding service
    let embedding_service = embedding::service::EmbeddingService::from_env()?;
    info!("Embedding service initialized (available: {})", embedding_service.is_available());

    let brain_service = brain::service::BrainService::new(arango_client, nats_client.clone(), embedding_service);
    let brain_handle = tokio::spawn(async move {
        if let Err(e) = brain_service.run().await {
            error!(error = %e, "brain service exited with error");
        }
    });
    info!("brain service started");

    // 4b. Spawn the tool registry service.
    let tool_registry_arango =
        brain::client::ArangoClient::new(&arango_url, &arango_database, &arango_password)?;
    let tool_registry_service =
        tool_registry::service::ToolRegistryService::new(tool_registry_arango);
    let tool_registry_handle = tool_registry_service.spawn(nats_client.clone()).await?;
    info!("capability registry service started");

    // 4c. Spawn the plugin storage service.
    let storage_arango =
        brain::client::ArangoClient::new(&arango_url, &arango_database, &arango_password)?;
    let storage_service = plugin_storage::service::PluginStorageService::new(storage_arango);
    let storage_handle = storage_service.spawn(nats_client.clone()).await?;
    info!("plugin storage service started");

    // 5. Spawn the scheduler service (decay + health monitoring).
    let scheduler_arango =
        brain::client::ArangoClient::new(&arango_url, &arango_database, &arango_password)?;
    let scheduler_service = scheduler::service::SchedulerService::new(
        scheduler_arango,
        nats_client.clone(),
        registry_for_scheduler,
    );
    let scheduler_handle = scheduler_service.spawn().await?;
    info!("scheduler service started");

    // 6. Spawn the workflow engine (replaces the old orchestrator).
    let agents_dir = platform_config
        .as_ref()
        .map(|c| c.agents_dir.as_str())
        .unwrap_or("agents/")
        .to_string();

    let workflows_dir = platform_config
        .as_ref()
        .map(|c| c.workflows_dir.as_str())
        .unwrap_or("workflows/")
        .to_string();

    let orchestrator = OrchestratorService::new();
    let orchestrator_handle = orchestrator
        .spawn(nats_client.clone(), &agents_dir, &workflows_dir)
        .await?;
    info!("workflow engine started");

    // 7. Spawn the consciousness service.
    let consciousness_service =
        consciousness::service::ConsciousnessService::new(nats_client.clone(), &agents_dir)?;
    let consciousness_handle = consciousness_service.spawn().await?;
    info!("consciousness service started");

    // 8. Spawn the trace collector service.
    let trace_collector = trace_collector::service::TraceCollectorService::new(
        nats_client.clone(),
        10000, // max traces in memory
    );
    let trace_collector_handle = trace_collector.spawn().await?;
    info!("trace collector service started");

    // 9. Load system skills from YAML files.
    let embedding_service = embedding::service::EmbeddingService::from_env()?;
    let skills_arango =
        brain::client::ArangoClient::new(&arango_url, &arango_database, &arango_password)?;
    let skills_dir = "skills/";
    if let Err(e) =
        embedding::skill_loader::load_system_skills(&skills_arango, &embedding_service, skills_dir)
            .await
    {
        warn!(error = %e, "Failed to load system skills (non-fatal)");
    }

    // 10. Wait for all services.
    tokio::select! {
        _ = registry_handle => {
            warn!("registry service exited unexpectedly");
        }
        _ = brain_handle => {
            warn!("brain service exited unexpectedly");
        }
        _ = tool_registry_handle => {
            warn!("capability registry service exited unexpectedly");
        }
        _ = storage_handle => {
            warn!("plugin storage service exited unexpectedly");
        }
        _ = scheduler_handle => {
            warn!("scheduler service exited unexpectedly");
        }
        _ = orchestrator_handle => {
            warn!("orchestrator service exited unexpectedly");
        }
        _ = consciousness_handle => {
            warn!("consciousness service exited unexpectedly");
        }
        _ = trace_collector_handle => {
            warn!("trace collector service exited unexpectedly");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("received Ctrl-C, shutting down...");
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
    let client = brain::client::ArangoClient::new(&arango_url, &arango_database, &arango_password)?;

    // 3. Run initialization.
    brain::init::initialize_brain(&client).await?;

    Ok(())
}

/// Run full validation of platform config and all agent definitions.
/// Returns true if everything is valid, false otherwise.
fn run_validate(config_path: &str) -> bool {
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut ok_checks: Vec<String> = Vec::new();

    // 1. Parse platform config
    let platform_path = Path::new(config_path);
    let platform_config = match load_platform_config(platform_path) {
        Ok(cfg) => {
            ok_checks.push(format!("Platform config: {}", platform_path.display()));
            Some(cfg)
        }
        Err(e) => {
            errors.push(format!("Platform config: {}", e));
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
        errors.push(format!(
            "Agents directory not found: {}",
            agents_path.display()
        ));
        print_validation_summary(&ok_checks, &warnings, &errors);
        return false;
    }

    // 3. Parse all agent YAML files
    let entries = match std::fs::read_dir(agents_path) {
        Ok(entries) => entries,
        Err(e) => {
            errors.push(format!("Cannot read agents directory: {}", e));
            print_validation_summary(&ok_checks, &warnings, &errors);
            return false;
        }
    };

    let mut agent_configs: Vec<(String, AgentConfigFile)> = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                errors.push(format!("Error reading directory entry: {}", e));
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
                ok_checks.push(format!(
                    "Agent config: {} (id: {})",
                    path.display(),
                    agent_file.agent.id
                ));
                agent_configs.push((path.display().to_string(), agent_file));
            }
            Err(e) => {
                errors.push(format!("Agent config {}: {}", path.display(), e));
            }
        }
    }

    // 4. Check for duplicate agent IDs across YAML files
    {
        let mut seen_ids: HashMap<String, String> = HashMap::new();
        for (source_path, agent_file) in &agent_configs {
            let agent_id = &agent_file.agent.id;
            if let Some(first_path) = seen_ids.get(agent_id) {
                errors.push(format!(
                    "Duplicate agent ID '{}': found in both '{}' and '{}'",
                    agent_id, first_path, source_path
                ));
            } else {
                seen_ids.insert(agent_id.clone(), source_path.clone());
            }
        }
    }

    // 5. Validate pipeline event type chains using OrchestratorService logic
    let orchestrator = OrchestratorService::new();
    for (source_path, agent_file) in &agent_configs {
        let agent = &agent_file.agent;
        let pipeline_warnings = orchestrator.validate_pipeline(agent);
        for w in &pipeline_warnings {
            warnings.push(format!("Agent '{}' ({}): {}", agent.id, source_path, w));
        }
        if pipeline_warnings.is_empty() {
            ok_checks.push(format!(
                "Pipeline chain: agent '{}' event types are coherent",
                agent.id
            ));
        }
    }

    // 6. Check that scopes follow the naming convention (scope_*)
    for (source_path, agent_file) in &agent_configs {
        let agent = &agent_file.agent;

        // Collect all scopes referenced by this agent
        let mut all_scopes: Vec<&str> = vec![agent.scope.as_str()];
        for s in &agent.additional_scopes {
            all_scopes.push(s.as_str());
        }

        for scope_name in &all_scopes {
            if !scope_name.starts_with("scope_") {
                warnings.push(format!(
                    "Agent '{}' ({}): scope '{}' does not follow naming convention 'scope_*'",
                    agent.id, source_path, scope_name
                ));
            }
        }

        ok_checks.push(format!(
            "Scopes: agent '{}' references {} scope(s)",
            agent.id,
            all_scopes.len()
        ));
    }

    // 7. Check that prompt files referenced by agents exist
    for (source_path, agent_file) in &agent_configs {
        let agent = &agent_file.agent;

        // Check pipeline steps for prompt references
        for step in &agent.pipeline.steps {
            if let Some(config) = &step.config {
                if let Some(prompt_val) = config.get("prompt") {
                    if let Some(prompt_path) = prompt_val.as_str() {
                        let prompt_file = Path::new(prompt_path);
                        if prompt_file.exists() {
                            ok_checks.push(format!(
                                "Prompt file: {} (agent '{}', step '{}')",
                                prompt_path, agent.id, step.plugin
                            ));
                        } else {
                            errors.push(format!(
                                "Prompt file not found: {} (agent '{}' in {}, step '{}')",
                                prompt_path, agent.id, source_path, step.plugin
                            ));
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
                                ok_checks.push(format!(
                                    "Prompt file: {} (agent '{}', background step '{}')",
                                    prompt_path, agent.id, step.plugin
                                ));
                            } else {
                                errors.push(format!(
                                    "Prompt file not found: {} (agent '{}' in {}, background step '{}')",
                                    prompt_path, agent.id, source_path, step.plugin
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    // 8. Check that referenced plugins are known
    {
        let known_plugins: HashSet<&str> = [
            "content-ingester",
            "graph-context-loader",
            "llm-router",
            "llm-google",
            "llm-openai",
            "llm-anthropic",
            "llm-ollama",
            "tool-dispatcher",
            "event-emitter",
            "response-formatter",
            "entity-extractor",
            "fact-extractor",
            "scope-classifier",
            "task-detector",
            "code-executor",
            "calendar",
            "email",
            "notification",
            "telegram",
            "cli",
            "claude-code",
            "api-gateway",
            "feedback-extractor",
        ]
        .into_iter()
        .collect();

        for (source_path, agent_file) in &agent_configs {
            let agent = &agent_file.agent;
            for step in &agent.pipeline.steps {
                if !known_plugins.contains(step.plugin.as_str()) {
                    warnings.push(format!(
                        "Agent '{}' ({}): pipeline step references unknown plugin '{}'",
                        agent.id, source_path, step.plugin
                    ));
                }
            }
            if let Some(bg_steps) = &agent.background {
                for step in bg_steps {
                    if !known_plugins.contains(step.plugin.as_str()) {
                        warnings.push(format!(
                            "Agent '{}' ({}): background step references unknown plugin '{}'",
                            agent.id, source_path, step.plugin
                        ));
                    }
                }
            }
        }
    }

    // Print summary
    print_validation_summary(&ok_checks, &warnings, &errors);

    errors.is_empty()
}

/// Print a clear validation summary with all checks, warnings, and errors.
fn print_validation_summary(ok_checks: &[String], warnings: &[String], errors: &[String]) {
    println!();
    println!("=== Seidrum Validation Summary ===");
    println!();

    for check in ok_checks {
        info!("[OK]   {}", check);
    }

    for w in warnings {
        warn!("[WARN] {}", w);
    }

    for e in errors {
        error!("[FAIL] {}", e);
    }

    println!();
    println!(
        "Results: {} passed, {} warnings, {} errors",
        ok_checks.len(),
        warnings.len(),
        errors.len()
    );

    if errors.is_empty() && warnings.is_empty() {
        println!("Status: ALL OK");
    } else if errors.is_empty() {
        println!("Status: PASS (with warnings)");
    } else {
        println!("Status: FAILED");
    }
    println!();
}
