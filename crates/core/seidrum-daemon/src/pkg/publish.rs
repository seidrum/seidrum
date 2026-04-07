use super::{PackageManifest, RegistriesConfig};
use crate::paths::SeidrumPaths;
use anyhow::Result;
use std::env;
use std::fs;
use std::path::Path;
use tracing::info;

/// Publish current directory as a package to the registry
pub fn publish(registry_name: &str, paths: &SeidrumPaths) -> Result<()> {
    println!("Publishing package from current directory...");

    let cwd = env::current_dir()?;

    // Check for seidrum-pkg.yaml
    let manifest_path = cwd.join("seidrum-pkg.yaml");
    if !manifest_path.exists() {
        anyhow::bail!(
            "No seidrum-pkg.yaml found in current directory. Please create one first."
        );
    }

    // Load and validate manifest
    let manifest_content = fs::read_to_string(&manifest_path)?;
    let manifest: PackageManifest = serde_yaml::from_str(&manifest_content)?;

    println!("\nPackage Details:");
    println!("  Name:    {}", manifest.name);
    println!("  Version: {}", manifest.version);
    println!("  Kind:    {:?}", manifest.kind);

    // Get registry
    let registries_yaml = paths.seidrum_home.join("registries.yaml");
    let registries_config: RegistriesConfig = if registries_yaml.exists() {
        let content = fs::read_to_string(&registries_yaml)?;
        serde_yaml::from_str(&content).unwrap_or_default()
    } else {
        RegistriesConfig::default()
    };

    let registry = registries_config
        .find(registry_name)
        .ok_or_else(|| {
            anyhow::anyhow!("Registry '{}' not configured", registry_name)
        })?;

    println!("  Registry: {} ({})", registry.name, registry.url);

    // Prepare registry directory
    let registry_path = paths.registries_dir().join(&registry.name);
    let packages_dir = registry_path.join("packages").join(&manifest.name);
    fs::create_dir_all(&packages_dir)?;

    // Copy manifest
    let dest_manifest = packages_dir.join("seidrum-pkg.yaml");
    fs::copy(&manifest_path, &dest_manifest)?;
    info!("Copied manifest to {}", dest_manifest.display());

    // Copy artifacts if they exist
    let artifacts_dir = cwd.join("artifacts");
    if artifacts_dir.exists() {
        let dest_artifacts = packages_dir.join("artifacts");
        fs::create_dir_all(&dest_artifacts)?;
        copy_dir_all(&artifacts_dir, &dest_artifacts)?;
        info!("Copied artifacts to {}", dest_artifacts.display());
    }

    println!("\nPackage published locally to {}!", packages_dir.display());
    println!("\nTo publish to the registry repository:");
    println!("  1. cd {}", registry_path.display());
    println!("  2. git add packages/{}", manifest.name);
    println!("  3. git commit -m 'Publish {} v{}'", manifest.name, manifest.version);
    println!("  4. git push");

    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src_path = entry.path();
        let file_name = entry.file_name();
        let dst_path = dst.join(&file_name);

        if ty.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
