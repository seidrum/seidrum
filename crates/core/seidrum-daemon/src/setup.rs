//! Interactive first-run setup wizard.
//!
//! Downloads NATS, pulls ArangoDB Docker image, generates .env,
//! initializes the database, and writes infra config.

use anyhow::{Context, Result};
use dialoguer::{Confirm, Input};
use tracing::warn;

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
            anyhow::bail!(
                "Docker or Podman is required. Install one and run 'seidrum setup' again."
            );
        }
    }
    println!();

    // 2. Download NATS binary

    // 3. Prompt for ArangoDB password only
    let arango_password = if use_defaults {
        generate_password(16)
    } else {
        let default_pw = generate_password(16);
        Input::new()
            .with_prompt("ArangoDB root password")
            .default(default_pw)
            .interact_text()?
    };

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
        nats: NatsConfig {
            mode: NatsMode::Native,
            version: "2.10.24".to_string(),
            port: 4222,
            http_port: 8222,
        },
        arango: arango_config,
        container_runtime: runtime,
    };

    let infra = InfraManager::new(
        infra_config.clone(),
        SeidrumPaths::resolve(&paths.config_dir),
    );

    // 5. Download NATS
    // No NATS to download (eventbus is built into the kernel)

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
            write_env_file(&env_path, &arango_password)?;
        }
    } else {
        write_env_file(&env_path, &arango_password)?;
    }

    // 8. Start infrastructure temporarily for DB init
    println!("Starting infrastructure for database initialization...");
    // No NATS to start
    infra.start_arango()?;
    infra.wait_for_healthy().await?;

    // 9. Initialize the brain database
    println!("Initializing database...");
    if let Err(e) = crate::daemon::run_kernel_command(paths, &["init"]).await {
        warn!(error = %e, "Database initialization failed (kernel may not be built yet)");
        println!("  Warning: Database init failed. Run 'seidrum init' after building the kernel.");
    }

    // 10. All plugins and agents start disabled by default.
    // The user enables what they need via the dashboard or CLI.
    // No auto-enable logic — the user is in control.

    // 11. Stop infrastructure (user will use `seidrum start`)
    // No NATS to stop
    infra.stop_arango()?;

    // 12. Save infra config
    infra_config.save(paths)?;

    // 13. Print summary
    println!();
    println!("╔══════════════════════════════════════╗");
    println!("║       Infrastructure Ready!          ║");
    println!("╚══════════════════════════════════════╝");
    println!();
    println!("Next steps:");
    println!("  1. seidrum start");
    println!("  2. Open http://localhost:3030/dashboard");
    println!("  3. Follow the setup wizard to configure your first channel");
    println!();

    Ok(())
}

/// Generate a random alphanumeric password.
fn generate_password(len: usize) -> String {
    use rand::distributions::Alphanumeric;
    use rand::Rng;
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

/// Write the .env file with infrastructure secrets only.
fn write_env_file(path: &std::path::Path, arango_password: &str) -> Result<()> {
    let content = format!(
        r#"# Seidrum environment configuration
# Generated by 'seidrum setup'

# ArangoDB
ARANGO_PASSWORD={arango_password}

# Additional secrets and API keys are configured via the dashboard
"#
    );

    std::fs::write(path, content).context("Failed to write .env file")?;
    println!("  Wrote {}", path.display());
    Ok(())
}
