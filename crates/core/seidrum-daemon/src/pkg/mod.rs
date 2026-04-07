pub mod download;
pub mod install;
pub mod list;
pub mod publish;
pub mod registry;
pub mod resolve;
pub mod search;
pub mod uninstall;
pub mod verify;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Package manifest file (seidrum-pkg.yaml)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PackageManifest {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub author: Option<String>,
    pub kind: PackageKind,
    pub artifacts: Vec<PackageArtifact>,
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub env_vars: HashMap<String, String>,
    #[serde(default)]
    pub dependencies: Vec<PackageDependency>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "lowercase")]
pub enum PackageKind {
    Plugin,
    Agent,
    Bundle,
}

/// A pre-built binary artifact for a specific platform.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PackageArtifact {
    pub target: String,
    pub url: String,
    #[serde(default)]
    pub image: Option<String>,
    pub sha256: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PackageDependency {
    pub name: String,
    pub version: Option<String>,
}

/// Resolved package with source information
#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub manifest: PackageManifest,
    pub source: PackageSource,
    pub manifest_url: String,
}

#[derive(Debug, Clone)]
pub enum PackageSource {
    Registry { name: String, version: String },
    Url(String),
    Local(PathBuf),
}

/// Installed package metadata
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct InstalledPackage {
    pub name: String,
    pub version: String,
    pub kind: PackageKind,
    pub installed_at: String,
    pub source: String, // Registry name or URL
}

/// Installed packages registry
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct InstalledPackagesRegistry {
    pub packages: Vec<InstalledPackage>,
}

impl InstalledPackagesRegistry {
    pub fn find(&self, name: &str) -> Option<&InstalledPackage> {
        self.packages.iter().find(|p| p.name == name)
    }

    pub fn add(&mut self, pkg: InstalledPackage) {
        // Remove old version if exists
        self.packages.retain(|p| p.name != pkg.name);
        self.packages.push(pkg);
    }

    pub fn remove(&mut self, name: &str) {
        self.packages.retain(|p| p.name != name);
    }
}

/// Registry configuration
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RegistryConfig {
    pub name: String,
    pub url: String,
    pub synced_at: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct RegistriesConfig {
    pub registries: Vec<RegistryConfig>,
}

impl RegistriesConfig {
    pub fn find(&self, name: &str) -> Option<&RegistryConfig> {
        self.registries.iter().find(|r| r.name == name)
    }

    pub fn add(&mut self, config: RegistryConfig) {
        // Replace if exists
        self.registries.retain(|r| r.name != config.name);
        self.registries.push(config);
    }

    pub fn remove(&mut self, name: &str) {
        self.registries.retain(|r| r.name != name);
    }
}
