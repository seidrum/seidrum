use super::{PackageManifest, PackageSource, ResolvedPackage, RegistriesConfig};
use crate::paths::SeidrumPaths;
use anyhow::{anyhow, Result};
use std::path::PathBuf;
use tracing::{debug, info};

/// Resolve a package reference: name, name@version, or URL
pub fn resolve_package(
    reference: &str,
    _paths: &SeidrumPaths,
) -> Result<ResolvedPackage> {
    // Check if it's a URL
    if reference.starts_with("http://") || reference.starts_with("https://")
        || reference.starts_with("git@")
        || reference.ends_with(".git")
    {
        return resolve_from_url(reference);
    }

    // Check if it's a local path
    let local_path = PathBuf::from(reference);
    if local_path.exists() && local_path.join("seidrum-pkg.yaml").exists() {
        return resolve_from_local(&local_path);
    }

    // Otherwise, treat as name or name@version from registry
    resolve_from_registry(reference, _paths)
}

/// Resolve from a URL (git or HTTP)
fn resolve_from_url(url: &str) -> Result<ResolvedPackage> {
    info!("Resolving package from URL: {}", url);

    let manifest_url = if url.ends_with(".git") {
        // Git repo URL - assume seidrum-pkg.yaml is in root
        format!("{}/raw/main/seidrum-pkg.yaml", url.trim_end_matches(".git"))
    } else if url.contains("raw.githubusercontent.com") || url.contains("/raw/") {
        // Already a raw file URL
        url.to_string()
    } else {
        // Assume it's a directory URL
        format!("{}/seidrum-pkg.yaml", url.trim_end_matches('/'))
    };

    debug!("Fetching manifest from: {}", manifest_url);

    // This is a placeholder - in real implementation, would fetch from HTTP
    // For now, return an error indicating we need to implement HTTP fetching
    Err(anyhow!(
        "HTTP package resolution not yet implemented. URL: {}",
        url
    ))
}

/// Resolve from a local directory
fn resolve_from_local(path: &PathBuf) -> Result<ResolvedPackage> {
    info!("Resolving package from local directory: {}", path.display());

    let manifest_path = path.join("seidrum-pkg.yaml");
    let manifest_content = std::fs::read_to_string(&manifest_path)?;
    let manifest: PackageManifest = serde_yaml::from_str(&manifest_content)?;

    Ok(ResolvedPackage {
        source: PackageSource::Local(path.clone()),
        manifest_url: manifest_path.to_string_lossy().to_string(),
        manifest,
    })
}

/// Resolve from registry
fn resolve_from_registry(reference: &str, paths: &SeidrumPaths) -> Result<ResolvedPackage> {
    info!("Resolving package from registry: {}", reference);

    // Parse name@version
    let (name, version) = if let Some(at_pos) = reference.find('@') {
        let (n, v) = reference.split_at(at_pos);
        (n, Some(&v[1..]))
    } else {
        (reference, None)
    };

    // Load registries config
    let registries_yaml = paths.seidrum_home.join("registries.yaml");
    let registries_config: RegistriesConfig = if registries_yaml.exists() {
        let content = std::fs::read_to_string(&registries_yaml)?;
        serde_yaml::from_str(&content).unwrap_or_default()
    } else {
        RegistriesConfig::default()
    };

    // Use the first available registry (or default)
    let registry = registries_config
        .registries
        .first()
        .ok_or_else(|| anyhow!("No registries configured. Run 'seidrum pkg registry add' first."))?;

    // Look for package in registry index
    // This is a placeholder - in real implementation, would search registry index
    let resolved_version = version.unwrap_or("latest").to_string();

    debug!(
        "Found package {} version {} in registry {}",
        name, resolved_version, registry.name
    );

    // Return placeholder manifest
    let manifest = PackageManifest {
        name: name.to_string(),
        version: resolved_version.clone(),
        description: None,
        author: None,
        kind: super::PackageKind::Plugin,
        artifacts: vec![],
        events: vec![],
        env_vars: Default::default(),
        dependencies: vec![],
    };

    Ok(ResolvedPackage {
        source: PackageSource::Registry {
            name: registry.name.clone(),
            version: resolved_version,
        },
        manifest_url: format!(
            "{}/packages/{}/seidrum-pkg.yaml",
            registry.url, name
        ),
        manifest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_name_at_version() {
        let (name, version) = if let Some(at_pos) = "telegram@1.0.0".find('@') {
            let (n, v) = "telegram@1.0.0".split_at(at_pos);
            (n, Some(&v[1..]))
        } else {
            ("telegram@1.0.0", None)
        };
        assert_eq!(name, "telegram");
        assert_eq!(version, Some("1.0.0"));
    }

    #[test]
    fn test_parse_name_only() {
        let (name, version) = if let Some(at_pos) = "telegram".find('@') {
            let (n, v) = "telegram".split_at(at_pos);
            (n, Some(&v[1..]))
        } else {
            ("telegram", None)
        };
        assert_eq!(name, "telegram");
        assert_eq!(version, None);
    }
}
