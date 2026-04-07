use super::{
    download, resolve, verify, InstalledPackage, InstalledPackagesRegistry, PackageKind,
};
use crate::paths::SeidrumPaths;
use anyhow::Result;
use chrono::Utc;
use dialoguer::Confirm;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, info};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Main install command
pub async fn install(package_str: &str, yes: bool, paths: &SeidrumPaths) -> Result<()> {
    println!("Resolving package: {}", package_str);

    // Step 1: Resolve package
    let resolved = resolve::resolve_package(package_str, paths)?;
    let manifest = &resolved.manifest;

    println!("\nPackage Details:");
    println!("  Name:        {}", manifest.name);
    println!("  Version:     {}", manifest.version);
    if let Some(desc) = &manifest.description {
        println!("  Description: {}", desc);
    }
    println!("  Kind:        {:?}", manifest.kind);
    println!("  Events:      {:?}", manifest.events);
    if !manifest.env_vars.is_empty() {
        println!("  Environment Variables:");
        for (k, v) in &manifest.env_vars {
            println!("    {} = {}", k, v);
        }
    }

    // Step 2: Show confirmation
    if !yes {
        let confirm = Confirm::new()
            .with_prompt("Install this package?")
            .default(true)
            .interact()?;

        if !confirm {
            println!("Installation cancelled.");
            return Ok(());
        }
    }

    println!("\nInstalling {}...", manifest.name);

    // Step 3: Download and install based on kind
    match manifest.kind {
        PackageKind::Plugin => install_plugin(&resolved, paths).await?,
        PackageKind::Agent => install_agent(&resolved, paths)?,
        PackageKind::Bundle => install_bundle(&resolved, paths).await?,
    }

    // Step 4: Record in installed.yaml
    let installed_pkg = InstalledPackage {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        kind: manifest.kind.clone(),
        installed_at: Utc::now().to_rfc3339(),
        source: match &resolved.source {
            super::PackageSource::Registry { name, .. } => name.clone(),
            super::PackageSource::Url(url) => url.clone(),
            super::PackageSource::Local(path) => path.to_string_lossy().to_string(),
        },
    };

    let installed_yaml = paths.installed_yaml();
    let mut registry: InstalledPackagesRegistry = if installed_yaml.exists() {
        let content = fs::read_to_string(&installed_yaml)?;
        serde_yaml::from_str(&content).unwrap_or_default()
    } else {
        InstalledPackagesRegistry::default()
    };

    registry.add(installed_pkg);
    let yaml_content = serde_yaml::to_string(&registry)?;
    fs::write(&installed_yaml, yaml_content)?;

    println!("\nSuccessfully installed {} v{}", manifest.name, manifest.version);
    Ok(())
}

async fn install_plugin(resolved: &super::ResolvedPackage, paths: &SeidrumPaths) -> Result<()> {
    let manifest = &resolved.manifest;

    // Download artifact
    let temp_dir = paths.seidrum_home.join("temp");
    fs::create_dir_all(&temp_dir)?;

    let artifact_path = download::download_artifact(&manifest.artifacts, &temp_dir).await?;
    debug!("Downloaded artifact to {}", artifact_path.display());

    // Find the artifact for the current platform
    let target = download::current_target();
    let artifact = manifest.artifacts.iter()
        .find(|a| a.target == target)
        .ok_or_else(|| anyhow::anyhow!("No artifact for target {}", target))?;

    // Verify against the SPECIFIC artifact that was downloaded
    if !verify::verify_sha256(&artifact_path, &artifact.sha256)? {
        anyhow::bail!("SHA-256 verification failed");
    }

    // Extract plugin binary
    let extract_dir = paths.seidrum_home.join("temp").join("extract");
    fs::create_dir_all(&extract_dir)?;

    extract_tar_gz(&artifact_path, &extract_dir)?;
    debug!("Extracted artifact to {}", extract_dir.display());

    // Find binary in extracted files
    let binary_path = find_executable(&extract_dir, &manifest.name)?;
    let bin_dir = paths.managed_bin_dir();
    fs::create_dir_all(&bin_dir)?;

    let dest_path = bin_dir.join(&manifest.name);
    fs::copy(&binary_path, &dest_path)?;
    fs::set_permissions(&dest_path, fs::Permissions::from_mode(0o755))?;
    println!("  Installed plugin binary to {}", dest_path.display());

    // Add to plugins.yaml if it exists
    add_plugin_to_config(&manifest.name, &manifest.name, paths)?;

    // Cleanup
    let _ = fs::remove_dir_all(&extract_dir);
    let _ = fs::remove_file(&artifact_path);

    Ok(())
}

fn install_agent(resolved: &super::ResolvedPackage, paths: &SeidrumPaths) -> Result<()> {
    let manifest = &resolved.manifest;

    // For agents, we copy the manifest and prompts
    // This is a simplified version - in reality would handle more complex scenarios
    let agents_dir = paths.config_dir.join("agents");
    fs::create_dir_all(&agents_dir)?;

    // Create a basic agent YAML with all required fields
    let agent_yaml = format!(
        "agent:\n  id: {}\n  description: {}\n  enabled: false\n  scope: \"default\"\n  subscribe:\n    - \"channel.*.inbound\"\n  prompt: \"prompts/{}.md\"\n  tools: []\n",
        manifest.name,
        manifest.description.as_deref().unwrap_or(""),
        manifest.name
    );

    let agent_file = agents_dir.join(format!("{}.yaml", manifest.name));
    fs::write(&agent_file, agent_yaml)?;
    println!("  Installed agent config to {}", agent_file.display());

    // Write placeholder prompt file
    let prompts_dir = agents_dir.parent()
        .unwrap_or(&agents_dir)
        .join("prompts");
    fs::create_dir_all(&prompts_dir)?;
    let prompt_path = prompts_dir.join(format!("{}.md", manifest.name));
    if !prompt_path.exists() {
        fs::write(&prompt_path, format!(
            "# {}\n\n{}\n\nYou are a helpful AI assistant.\n",
            manifest.name,
            manifest.description.as_deref().unwrap_or("")
        ))?;
        info!("Created placeholder prompt: {}", prompt_path.display());
    }

    Ok(())
}

async fn install_bundle(resolved: &super::ResolvedPackage, paths: &SeidrumPaths) -> Result<()> {
    let manifest = &resolved.manifest;

    // Install dependencies recursively
    for dep in &manifest.dependencies {
        let dep_ref = if let Some(ver) = &dep.version {
            format!("{}@{}", dep.name, ver)
        } else {
            dep.name.clone()
        };

        println!("  Installing dependency: {}", dep.name);
        install(&dep_ref, true, paths).await?;
    }

    println!("  Bundle dependencies installed");
    Ok(())
}

fn add_plugin_to_config(plugin_name: &str, binary_name: &str, paths: &SeidrumPaths) -> Result<()> {
    let plugins_yaml = paths.plugins_yaml();

    let mut doc: serde_yaml::Value = if plugins_yaml.exists() {
        let content = fs::read_to_string(&plugins_yaml)?;
        serde_yaml::from_str(&content).unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()))
    } else {
        serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
    };

    // Navigate to or create plugins map
    let root = doc.as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("plugins.yaml root is not a mapping"))?;

    let plugins_key = serde_yaml::Value::String("plugins".to_string());
    let plugins_map = root.entry(plugins_key)
        .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));

    let plugins_map = plugins_map.as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("plugins key is not a mapping"))?;

    // Add the plugin entry
    let mut entry = serde_yaml::Mapping::new();
    entry.insert(
        serde_yaml::Value::String("enabled".to_string()),
        serde_yaml::Value::Bool(false),
    );
    entry.insert(
        serde_yaml::Value::String("binary".to_string()),
        serde_yaml::Value::String(binary_name.to_string()),
    );

    plugins_map.insert(
        serde_yaml::Value::String(plugin_name.to_string()),
        serde_yaml::Value::Mapping(entry),
    );

    let yaml = serde_yaml::to_string(&doc)?;
    fs::write(&plugins_yaml, yaml)?;

    Ok(())
}

fn extract_tar_gz(tar_path: &Path, extract_to: &Path) -> Result<()> {
    debug!(
        "Extracting {} to {}",
        tar_path.display(),
        extract_to.display()
    );

    let tar_gz = std::fs::File::open(tar_path)?;
    let tar = flate2::read::GzDecoder::new(tar_gz);
    let mut archive = tar::Archive::new(tar);

    archive.unpack(extract_to)?;
    Ok(())
}

fn find_executable(dir: &Path, name: &str) -> Result<PathBuf> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_file() {
            if let Some(file_name) = path.file_name() {
                if file_name.to_string_lossy().contains(name) {
                    return Ok(path);
                }
            }
        } else if path.is_dir() {
            // Recursively search subdirectories
            if let Ok(result) = find_executable(&path, name) {
                return Ok(result);
            }
        }
    }

    anyhow::bail!("Could not find executable for plugin {}", name)
}
