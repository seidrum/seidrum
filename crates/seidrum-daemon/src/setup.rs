//! Interactive first-run setup wizard.
//!
//! Downloads NATS, pulls ArangoDB Docker image, generates .env,
//! initializes the database, and writes infra config.

use anyhow::{Context, Result};
use dialoguer::{Confirm, Input};
use rand::Rng;
use tracing::{info, warn};

use crate::config::{load_plugins_config, save_plugins_config};
use crate::infra::{
    ArangoConfig, ArangoMode, ContainerRuntime, InfraConfig, InfraManager, NatsConfig, NatsMode,
};
use crate::paths::SeidrumPaths;

/// Run the interactive setup wizard.
pub async fn run(paths: &SeidrumPaths, use_defaults: bool) -> Result<()> {
    println!();
    println!("  ╔══════════════════════════════════════╗");
    println!("  ║         Seidrum Setup Wizard         ║");
    println!("  ╚══════════════════════════════════════╝");
    println!();

    paths.ensure_dirs()?;

    // 1. Detect container runtime
    println!("Checking prerequisites...");
    let runtime = InfraManager::detect_container_runtime();
    match &runtime {
        Some(ContainerRuntime::Docker) => println!("  Docker: found"),
        Some(ContainerRuntime::Podman) => println!("  Podman: found"),
        None => {
            println!("  Docker/Podman: not found");
            println!();
            println!("ArangoDB requires Docker (or Podman) to run.");
            if cfg!(target_os = "macos") {
                println!("Install Docker Desktop: https://docker.com/products/docker-desktop");
            } else {
                println!("Install Docker: https://docs.docker.com/engine/install/");
                println!("Or Podman:      https://podman.io/getting-started/installation");
            }
            anyhow::bail!("Docker or Podman is required. Install one and run 'seidrum setup' again.");
        }
    }
    println!();

    // 2. Download NATS binary
    let nats_config = NatsConfig {
        mode: NatsMode::Native,
        version: crate::infra::NATS_VERSION.to_string(),
        port: 4222,
        http_port: 8222,
    };

    // 3. Prompt for credentials
    let arango_password = if use_defaults {
        generate_password(16)
    } else {
        let default_pw = generate_password(16);
        Input::new()
            .with_prompt("ArangoDB root password")
            .default(default_pw)
            .interact_text()?
    };

    let google_api_key = if use_defaults {
        String::new()
    } else {
        Input::new()
            .with_prompt("Google API key (blank to skip)")
            .allow_empty(true)
            .default(String::new())
            .interact_text()?
    };

    let openai_api_key = if use_defaults {
        String::new()
    } else {
        Input::new()
            .with_prompt("OpenAI API key (blank to skip)")
            .allow_empty(true)
            .default(String::new())
            .interact_text()?
    };

    let telegram_token = if use_defaults {
        String::new()
    } else {
        Input::new()
            .with_prompt("Telegram Bot token (blank to skip)")
            .allow_empty(true)
            .default(String::new())
            .interact_text()?
    };

    let gateway_api_key = generate_hex_key(32);

    // 4. Build infra config
    let arango_config = ArangoConfig {
        mode: ArangoMode::Docker,
        image: "arangodb:3.12".to_string(),
        port: 8529,
        container_name: "seidrum-arangodb".to_string(),
        volume_name: "seidrum-arango-data".to_string(),
        password: arango_password.clone(),
    };

    let infra_config = InfraConfig {
        nats: nats_config,
        arango: arango_config,
        container_runtime: runtime,
    };

    let infra = InfraManager::new(infra_config.clone(), SeidrumPaths::resolve(&paths.config_dir));

    // 5. Download NATS
    infra.download_nats().await?;

    // 6. Pull ArangoDB image
    println!("Pulling ArangoDB Docker image...");
    infra.pull_arango_image()?;
    println!();

    // 7. Write .env file
    let env_path = std::env::current_dir()?.join(".env");
    if env_path.exists() && !use_defaults {
        let overwrite = Confirm::new()
            .with_prompt(".env file already exists. Overwrite?")
            .default(false)
            .interact()?;
        if !overwrite {
            println!("Keeping existing .env file");
        } else {
            write_env_file(&env_path, &arango_password, &google_api_key, &openai_api_key, &telegram_token, &gateway_api_key)?;
        }
    } else {
        write_env_file(&env_path, &arango_password, &google_api_key, &openai_api_key, &telegram_token, &gateway_api_key)?;
    }

    // 8. Start infrastructure temporarily for DB init
    println!("Starting infrastructure for database initialization...");
    infra.start_nats()?;
    infra.start_arango()?;
    infra.wait_for_healthy().await?;

    // 9. Initialize the brain database
    println!("Initializing database...");
    if let Err(e) = crate::daemon::run_kernel_command(paths, &["init"]).await {
        warn!(error = %e, "Database initialization failed (kernel may not be built yet)");
        println!("  Warning: Database init failed. Run 'seidrum init' after building the kernel.");
    }

    // 10. Enable api-gateway, auto-disable plugins without keys
    let plugins_yaml = paths.plugins_yaml();
    if plugins_yaml.exists() {
        let mut config = load_plugins_config(&plugins_yaml)?;

        // Enable api-gateway
        if let Some(gw) = config.plugins.get_mut("api-gateway") {
            gw.enabled = true;
            info!("Enabled api-gateway plugin");
        }

        // Disable plugins that need missing API keys
        if telegram_token.is_empty() {
            if let Some(tg) = config.plugins.get_mut("telegram") {
                tg.enabled = false;
                info!("Disabled telegram plugin (no token provided)");
            }
        }
        if google_api_key.is_empty() {
            if let Some(llm) = config.plugins.get_mut("llm-google") {
                llm.enabled = false;
                info!("Disabled llm-google plugin (no API key provided)");
            }
        }

        save_plugins_config(&plugins_yaml, &config)?;
    }

    // 11. Stop infrastructure (user will use `seidrum start`)
    infra.stop_nats()?;
    infra.stop_arango()?;

    // 12. Save infra config
    infra_config.save(paths)?;

    // 13. Print summary
    println!();
    println!("  ╔══════════════════════════════════════╗");
    println!("  ║          Setup Complete!             ║");
    println!("  ╚══════════════════════════════════════╝");
    println!();
    println!("  Run:       seidrum start");
    println!("  Dashboard: http://localhost:8080/dashboard");
    println!("  API key:   {}", gateway_api_key);
    println!();

    Ok(())
}

/// Generate a random alphanumeric password.
fn generate_password(len: usize) -> String {
    use rand::distributions::Alphanumeric;
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

/// Generate a random hex string.
fn generate_hex_key(len: usize) -> String {
    let mut rng = rand::thread_rng();
    (0..len).map(|_| format!("{:x}", rng.gen::<u8>() % 16)).collect()
}

/// Write the .env file with provided values.
fn write_env_file(
    path: &std::path::Path,
    arango_password: &str,
    google_api_key: &str,
    openai_api_key: &str,
    telegram_token: &str,
    gateway_api_key: &str,
) -> Result<()> {
    let content = format!(
        r#"# Seidrum environment configuration
# Generated by 'seidrum setup'

# ArangoDB
ARANGO_PASSWORD={arango_password}

# LLM Providers
GOOGLE_API_KEY={google_api_key}
OPENAI_API_KEY={openai_api_key}
EMBEDDING_PROVIDER=openai

# Telegram
TELEGRAM_TOKEN={telegram_token}
TELEGRAM_ALLOWED_USERS=

# API Gateway
GATEWAY_API_KEY={gateway_api_key}
GATEWAY_LISTEN_ADDR=0.0.0.0:8080
"#
    );

    std::fs::write(path, content).context("Failed to write .env file")?;
    println!("  Wrote {}", path.display());
    Ok(())
}
