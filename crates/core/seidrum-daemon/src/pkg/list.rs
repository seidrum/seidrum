use super::InstalledPackagesRegistry;
use crate::paths::SeidrumPaths;
use anyhow::Result;
use std::fs;
use tabled::{Table, Tabled};

#[derive(Tabled)]
struct PackageRow {
    name: String,
    version: String,
    kind: String,
    source: String,
    installed_at: String,
}

/// List installed packages
pub fn list_packages(paths: &SeidrumPaths) -> Result<()> {
    let installed_yaml = paths.installed_yaml();

    if !installed_yaml.exists() {
        println!("No packages installed yet.");
        return Ok(());
    }

    let content = fs::read_to_string(&installed_yaml)?;
    let registry: InstalledPackagesRegistry = serde_yaml::from_str(&content)?;

    if registry.packages.is_empty() {
        println!("No packages installed yet.");
        return Ok(());
    }

    let rows: Vec<PackageRow> = registry
        .packages
        .iter()
        .map(|pkg| PackageRow {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
            kind: format!("{:?}", pkg.kind),
            source: pkg.source.clone(),
            installed_at: pkg.installed_at.clone(),
        })
        .collect();

    let table = Table::new(&rows);
    println!("{}", table);

    Ok(())
}

/// Show info about a specific package
pub fn show_package_info(name: &str, paths: &SeidrumPaths) -> Result<()> {
    let installed_yaml = paths.installed_yaml();

    if !installed_yaml.exists() {
        println!("No packages installed.");
        return Ok(());
    }

    let content = fs::read_to_string(&installed_yaml)?;
    let registry: InstalledPackagesRegistry = serde_yaml::from_str(&content)?;

    match registry.find(name) {
        Some(pkg) => {
            println!("Package: {}", pkg.name);
            println!("  Version:      {}", pkg.version);
            println!("  Kind:         {:?}", pkg.kind);
            println!("  Source:       {}", pkg.source);
            println!("  Installed At: {}", pkg.installed_at);
        }
        None => {
            println!("Package '{}' not found.", name);
        }
    }

    Ok(())
}
