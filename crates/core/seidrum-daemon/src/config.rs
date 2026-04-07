//! Plugin configuration: loading, env interpolation, enable/disable.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tabled::{Table, Tabled};

use crate::paths::SeidrumPaths;

/// Top-level plugins.yaml structure.
#[derive(Debug, Serialize, Deserialize)]
pub struct PluginsConfig {
    pub plugins: BTreeMap<String, PluginEntry>,
}

/// A single plugin entry in plugins.yaml.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PluginEntry {
    /// Binary name (e.g., "seidrum-telegram")
    pub binary: String,
    /// Whether this plugin is enabled
    pub enabled: bool,
    /// Plugin-specific environment variables
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Package tracking — set when this plugin was installed via `seidrum pkg install`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<PluginPackageRef>,
}

/// Reference to a package this plugin was installed from.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PluginPackageRef {
    pub name: String,
    pub version: String,
    pub source: String,
}

/// Load plugins.yaml from the config directory.
pub fn load_plugins_config(path: &Path) -> Result<PluginsConfig> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let config: PluginsConfig =
        serde_yaml::from_str(&contents).with_context(|| "Failed to parse plugins.yaml")?;
    Ok(config)
}

/// Save plugins.yaml back to disk.
pub fn save_plugins_config(path: &Path, config: &PluginsConfig) -> Result<()> {
    let contents = serde_yaml::to_string(config).context("Failed to serialize plugins.yaml")?;
    std::fs::write(path, contents)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

/// Resolve `${VAR}` and `${VAR:-default}` patterns in a string.
/// Scans left-to-right without re-processing already-resolved text.
pub fn resolve_env(value: &str) -> String {
    let mut result = value.to_string();
    let mut search_from = 0;

    while search_from < result.len() {
        let start = match result[search_from..].find("${") {
            Some(s) => search_from + s,
            None => break,
        };

        let end = match result[start..].find('}') {
            Some(e) => start + e,
            None => break,
        };

        let expr = &result[start + 2..end];
        let resolved = if let Some(pos) = expr.find(":-") {
            let var_name = &expr[..pos];
            let default = &expr[pos + 2..];
            std::env::var(var_name).unwrap_or_else(|_| default.to_string())
        } else {
            std::env::var(expr).unwrap_or_default()
        };

        let resolved_len = resolved.len();
        result = format!("{}{}{}", &result[..start], resolved, &result[end + 1..]);
        // Skip past the resolved value to avoid re-processing
        search_from = start + resolved_len;
    }
    result
}

/// Resolve all env vars in a plugin entry.
pub fn resolve_plugin_env(entry: &PluginEntry) -> BTreeMap<String, String> {
    entry
        .env
        .iter()
        .map(|(k, v)| (k.clone(), resolve_env(v)))
        .collect()
}

/// List all plugins in a formatted table.
pub fn list_plugins(paths: &SeidrumPaths) -> Result<()> {
    let config = load_plugins_config(&paths.plugins_yaml())?;

    #[derive(Tabled)]
    struct Row {
        #[tabled(rename = "Plugin")]
        name: String,
        #[tabled(rename = "Binary")]
        binary: String,
        #[tabled(rename = "Enabled")]
        enabled: String,
        #[tabled(rename = "Env Vars")]
        env_count: usize,
    }

    let rows: Vec<Row> = config
        .plugins
        .iter()
        .map(|(name, entry)| Row {
            name: name.clone(),
            binary: entry.binary.clone(),
            enabled: if entry.enabled {
                "yes".to_string()
            } else {
                "no".to_string()
            },
            env_count: entry.env.len(),
        })
        .collect();

    if rows.is_empty() {
        println!("No plugins configured in plugins.yaml");
    } else {
        println!("{}", Table::new(&rows));
    }

    Ok(())
}

/// Enable or disable a plugin in plugins.yaml.
pub fn set_enabled(paths: &SeidrumPaths, name: &str, enabled: bool) -> Result<()> {
    let yaml_path = paths.plugins_yaml();
    let mut config = load_plugins_config(&yaml_path)?;

    match config.plugins.get_mut(name) {
        Some(entry) => {
            entry.enabled = enabled;
            save_plugins_config(&yaml_path, &config)?;
            let state = if enabled { "enabled" } else { "disabled" };
            println!("Plugin '{}' {}", name, state);
            Ok(())
        }
        None => {
            anyhow::bail!("Plugin '{}' not found in plugins.yaml", name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_env_simple() {
        std::env::set_var("SEIDRUM_TEST_VAR", "hello");
        assert_eq!(resolve_env("${SEIDRUM_TEST_VAR}"), "hello");
        std::env::remove_var("SEIDRUM_TEST_VAR");
    }

    #[test]
    fn resolve_env_default() {
        std::env::remove_var("SEIDRUM_MISSING_VAR");
        assert_eq!(resolve_env("${SEIDRUM_MISSING_VAR:-fallback}"), "fallback");
    }

    #[test]
    fn resolve_env_no_match() {
        assert_eq!(resolve_env("plain text"), "plain text");
    }

    #[test]
    fn resolve_env_multiple() {
        std::env::set_var("SEIDRUM_A", "one");
        std::env::set_var("SEIDRUM_B", "two");
        assert_eq!(resolve_env("${SEIDRUM_A}-${SEIDRUM_B}"), "one-two");
        std::env::remove_var("SEIDRUM_A");
        std::env::remove_var("SEIDRUM_B");
    }

    #[test]
    fn yaml_roundtrip() {
        let yaml = r#"
plugins:
  telegram:
    binary: seidrum-telegram
    enabled: true
    env:
      TELEGRAM_TOKEN: "${TELEGRAM_TOKEN}"
  cli:
    binary: seidrum-cli
    enabled: false
"#;
        let config: PluginsConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.plugins.len(), 2);
        assert!(config.plugins["telegram"].enabled);
        assert!(!config.plugins["cli"].enabled);
    }
}
