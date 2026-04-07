use super::{RegistriesConfig, RegistryConfig};
use crate::paths::SeidrumPaths;
use anyhow::Result;
use std::fs;
use std::process::Command;
use tabled::{Table, Tabled};
use tracing::info;

#[derive(Tabled)]
struct RegistryRow {
    name: String,
    url: String,
    synced: String,
}

/// Add a custom registry
pub fn add_registry(name: &str, url: &str, paths: &SeidrumPaths) -> Result<()> {
    println!("Adding registry: {}", name);

    let registries_yaml = paths.seidrum_home.join("registries.yaml");
    let mut registries: RegistriesConfig = if registries_yaml.exists() {
        let content = fs::read_to_string(&registries_yaml)?;
        serde_yaml::from_str(&content).map_err(|e| {
            tracing::warn!("Failed to parse registries config: {}", e);
            anyhow::anyhow!("Failed to parse registries config: {}", e)
        })?
    } else {
        RegistriesConfig::default()
    };

    // Clone the registry git repo
    let registry_path = paths.registries_dir().join(name);
    if registry_path.exists() {
        anyhow::bail!("Registry '{}' already exists", name);
    }

    println!("Cloning registry from: {}", url);
    fs::create_dir_all(paths.registries_dir())?;

    let output = Command::new("git")
        .args(&["clone", url, registry_path.to_str().unwrap()])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to clone registry: {}", stderr);
    }

    // Add to registries config
    let config = RegistryConfig {
        name: name.to_string(),
        url: url.to_string(),
        synced_at: Some(chrono::Utc::now().to_rfc3339()),
    };

    registries.add(config);
    let yaml_content = serde_yaml::to_string(&registries)?;
    fs::write(&registries_yaml, yaml_content)?;

    info!("Added registry {}", name);
    println!("Successfully added registry '{}'", name);

    Ok(())
}

/// List configured registries
pub fn list_registries(paths: &SeidrumPaths) -> Result<()> {
    let registries_yaml = paths.seidrum_home.join("registries.yaml");

    if !registries_yaml.exists() {
        println!("No registries configured yet.");
        println!("Add one with: seidrum pkg registry add <name> <url>");
        return Ok(());
    }

    let content = fs::read_to_string(&registries_yaml)?;
    let registries: RegistriesConfig = serde_yaml::from_str(&content)?;

    if registries.registries.is_empty() {
        println!("No registries configured yet.");
        return Ok(());
    }

    let rows: Vec<RegistryRow> = registries
        .registries
        .iter()
        .map(|r| RegistryRow {
            name: r.name.clone(),
            url: r.url.clone(),
            synced: r
                .synced_at
                .as_deref()
                .unwrap_or("never")
                .to_string(),
        })
        .collect();

    let table = Table::new(&rows);
    println!("{}", table);

    Ok(())
}

/// Remove a custom registry
pub fn remove_registry(name: &str, paths: &SeidrumPaths) -> Result<()> {
    println!("Removing registry: {}", name);

    let registries_yaml = paths.seidrum_home.join("registries.yaml");
    if !registries_yaml.exists() {
        anyhow::bail!("No registries configured");
    }

    let content = fs::read_to_string(&registries_yaml)?;
    let mut registries: RegistriesConfig = serde_yaml::from_str(&content)?;

    if registries.find(name).is_none() {
        anyhow::bail!("Registry '{}' not found", name);
    }

    registries.remove(name);
    let yaml_content = serde_yaml::to_string(&registries)?;
    fs::write(&registries_yaml, yaml_content)?;

    // Remove registry directory
    let registry_path = paths.registries_dir().join(name);
    if registry_path.exists() {
        fs::remove_dir_all(&registry_path)?;
    }

    info!("Removed registry {}", name);
    println!("Successfully removed registry '{}'", name);

    Ok(())
}

/// Sync registry index
pub fn sync_registry(name: Option<&str>, paths: &SeidrumPaths) -> Result<()> {
    let registries_yaml = paths.seidrum_home.join("registries.yaml");
    if !registries_yaml.exists() {
        anyhow::bail!("No registries configured");
    }

    let content = fs::read_to_string(&registries_yaml)?;
    let mut registries: RegistriesConfig = serde_yaml::from_str(&content).map_err(|e| {
        tracing::warn!("Failed to parse registries config: {}", e);
        anyhow::anyhow!("Failed to parse registries config: {}", e)
    })?;

    let registries_to_sync = if let Some(n) = name {
        vec![n.to_string()]
    } else {
        registries.registries.iter().map(|r| r.name.clone()).collect()
    };

    for reg_name in registries_to_sync {
        println!("Syncing registry: {}", reg_name);

        let registry_path = paths.registries_dir().join(&reg_name);
        if !registry_path.exists() {
            println!("  Registry not cloned locally, skipping");
            continue;
        }

        let output = Command::new("git")
            .current_dir(&registry_path)
            .args(&["pull"])
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            println!("  Warning: git pull failed: {}", stderr);
        } else {
            // Update sync timestamp
            if let Some(reg) = registries
                .registries
                .iter_mut()
                .find(|r| r.name == reg_name)
            {
                reg.synced_at = Some(chrono::Utc::now().to_rfc3339());
            }
            println!("  Synced successfully");
        }
    }

    let yaml_content = serde_yaml::to_string(&registries)?;
    fs::write(&registries_yaml, yaml_content)?;

    info!("Sync complete");
    Ok(())
}
