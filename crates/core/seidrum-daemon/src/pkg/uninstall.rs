use super::InstalledPackagesRegistry;
use crate::paths::SeidrumPaths;
use anyhow::{anyhow, Result};
use dialoguer::Confirm;
use std::fs;
use tracing::info;

/// Uninstall a package
pub fn uninstall(name: &str, yes: bool, paths: &SeidrumPaths) -> Result<()> {
    println!("Looking up installed package: {}", name);

    // Load installed packages
    let installed_yaml = paths.installed_yaml();
    if !installed_yaml.exists() {
        return Err(anyhow!("No packages installed yet"));
    }

    let content = fs::read_to_string(&installed_yaml)?;
    let mut registry: InstalledPackagesRegistry = serde_yaml::from_str(&content)?;

    let pkg = registry
        .find(name)
        .cloned()
        .ok_or_else(|| anyhow!("Package {} not installed", name))?;

    println!("\nPackage Details:");
    println!("  Name:        {}", pkg.name);
    println!("  Version:     {}", pkg.version);
    println!("  Kind:        {:?}", pkg.kind);
    println!("  Source:      {}", pkg.source);

    // Confirm
    if !yes {
        let confirm = Confirm::new()
            .with_prompt("Uninstall this package?")
            .default(false)
            .interact()?;

        if !confirm {
            println!("Uninstall cancelled.");
            return Ok(());
        }
    }

    println!("\nUninstalling {}...", name);

    // Remove binary if it's a plugin
    let bin_path = paths.managed_bin_dir().join(name);
    if bin_path.exists() {
        fs::remove_file(&bin_path)?;
        println!("  Removed binary: {}", bin_path.display());
    }

    // Remove from plugins.yaml
    remove_plugin_from_config(paths, name)?;

    // Remove agent files if present
    let agents_dir = paths.config_dir.join("agents");
    if agents_dir.exists() {
        let agent_file = agents_dir.join(format!("{}.yaml", name));
        if agent_file.exists() {
            fs::remove_file(&agent_file)?;
            println!("  Removed agent config: {}", agent_file.display());
        }
    }

    // Update installed.yaml
    registry.remove(name);
    let yaml_content = serde_yaml::to_string(&registry)?;
    fs::write(&installed_yaml, yaml_content)?;

    info!("Uninstalled package {}", name);
    println!("\nSuccessfully uninstalled {}", name);

    Ok(())
}

fn remove_plugin_from_config(paths: &SeidrumPaths, plugin_name: &str) -> Result<()> {
    let plugins_yaml = paths.plugins_yaml();

    if plugins_yaml.exists() {
        let content = fs::read_to_string(&plugins_yaml)?;
        let mut plugins: serde_yaml::Value = serde_yaml::from_str(&content)?;

        if let Some(enabled) = plugins.get_mut("enabled") {
            if let serde_yaml::Value::Sequence(seq) = enabled {
                seq.retain(|v| v.as_str().map_or(true, |s| s != plugin_name));
                let yaml = serde_yaml::to_string(&plugins)?;
                fs::write(&plugins_yaml, yaml)?;
            }
        }
    }

    Ok(())
}
