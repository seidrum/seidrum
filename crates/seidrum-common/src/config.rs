//! Shared configuration types for Seidrum platform and agent definitions.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

// ---------------------------------------------------------------------------
// Platform configuration (config/platform.yaml)
// ---------------------------------------------------------------------------

/// Top-level platform configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformConfig {
    /// NATS server URL.
    pub nats_url: String,

    /// ArangoDB server URL.
    pub arango_url: String,

    /// ArangoDB database name.
    #[serde(default = "default_arango_database")]
    pub arango_database: String,

    /// Directory containing agent YAML definitions.
    #[serde(default = "default_agents_dir")]
    pub agents_dir: String,

    /// Logging level (trace, debug, info, warn, error).
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

fn default_arango_database() -> String {
    "seidrum".to_string()
}

fn default_agents_dir() -> String {
    "agents/".to_string()
}

fn default_log_level() -> String {
    "info".to_string()
}

// ---------------------------------------------------------------------------
// Agent configuration (agents/*.yaml)
// ---------------------------------------------------------------------------

/// Top-level wrapper: the YAML has `agent:` as the root key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfigFile {
    pub agent: AgentConfig,
}

/// Full agent definition matching AGENT_SPEC.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Unique identifier used in events and logging.
    pub id: String,

    /// Human-readable name.
    pub name: String,

    /// Description for discovery and documentation.
    #[serde(default)]
    pub description: Option<String>,

    /// Whether this agent activates on kernel boot.
    pub enabled: bool,

    /// Primary scope.
    pub scope: String,

    /// Additional scopes this agent can access.
    #[serde(default)]
    pub additional_scopes: Vec<String>,

    /// The main request/response pipeline.
    pub pipeline: Pipeline,

    /// Background processing steps (async, non-blocking).
    #[serde(default)]
    pub background: Option<Vec<BackgroundStep>>,

    /// Rate limiting configuration.
    #[serde(default)]
    pub rate_limit: Option<RateLimit>,
}

/// Pipeline definition with triggers and sequential steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pipeline {
    /// NATS subjects that start this pipeline (supports wildcards).
    pub triggers: Vec<String>,

    /// Sequential processing steps.
    pub steps: Vec<PipelineStep>,
}

/// A single step in the agent pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineStep {
    /// Plugin identifier that handles this step.
    pub plugin: String,

    /// Event type this step consumes.
    pub consumes: String,

    /// Event type(s) this step produces.
    pub produces: StringOrVec,

    /// Per-step configuration passed to the plugin.
    #[serde(default)]
    pub config: Option<HashMap<String, serde_json::Value>>,
}

/// A background processing step (same structure as PipelineStep).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundStep {
    /// Plugin identifier that handles this step.
    pub plugin: String,

    /// Event type this step consumes.
    pub consumes: String,

    /// Event type(s) this step produces.
    pub produces: StringOrVec,

    /// Per-step configuration passed to the plugin.
    #[serde(default)]
    pub config: Option<HashMap<String, serde_json::Value>>,
}

/// Supports both a single string and a list of strings for the `produces` field.
///
/// In YAML this allows both:
///   produces: brain.content.stored
///   produces:
///     - task.created
///     - brain.fact.upsert
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StringOrVec {
    Single(String),
    Multiple(Vec<String>),
}

impl StringOrVec {
    /// Returns all values as a Vec, regardless of variant.
    pub fn as_vec(&self) -> Vec<&str> {
        match self {
            StringOrVec::Single(s) => vec![s.as_str()],
            StringOrVec::Multiple(v) => v.iter().map(|s| s.as_str()).collect(),
        }
    }
}

/// Rate limiting configuration for an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimit {
    /// Maximum LLM calls per minute.
    pub max_calls_per_minute: u32,

    /// Maximum daily spend in USD.
    pub max_daily_spend_usd: f64,
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Load and parse the platform configuration from a YAML file.
pub fn load_platform_config(path: &Path) -> anyhow::Result<PlatformConfig> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read platform config at {}: {}", path.display(), e))?;
    let config: PlatformConfig = serde_yaml::from_str(&contents)
        .map_err(|e| anyhow::anyhow!("Failed to parse platform config at {}: {}", path.display(), e))?;
    Ok(config)
}

/// Load and parse an agent configuration from a YAML file.
pub fn load_agent_config(path: &Path) -> anyhow::Result<AgentConfigFile> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read agent config at {}: {}", path.display(), e))?;
    let config: AgentConfigFile = serde_yaml::from_str(&contents)
        .map_err(|e| anyhow::anyhow!("Failed to parse agent config at {}: {}", path.display(), e))?;
    Ok(config)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_config_defaults() {
        let yaml = r#"
nats_url: nats://localhost:4222
arango_url: http://localhost:8529
"#;
        let config: PlatformConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.nats_url, "nats://localhost:4222");
        assert_eq!(config.arango_url, "http://localhost:8529");
        assert_eq!(config.arango_database, "seidrum");
        assert_eq!(config.agents_dir, "agents/");
        assert_eq!(config.log_level, "info");
    }

    #[test]
    fn test_platform_config_overrides() {
        let yaml = r#"
nats_url: nats://nats:4222
arango_url: http://arangodb:8529
arango_database: my_db
agents_dir: my_agents/
log_level: debug
"#;
        let config: PlatformConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.arango_database, "my_db");
        assert_eq!(config.agents_dir, "my_agents/");
        assert_eq!(config.log_level, "debug");
    }

    #[test]
    fn test_agent_config_full() {
        let yaml = r#"
agent:
  id: personal-assistant
  name: Personal Assistant
  description: General-purpose assistant
  enabled: true
  scope: scope_root
  additional_scopes:
    - scope_job_search
  pipeline:
    triggers:
      - channel.telegram.inbound
    steps:
      - plugin: llm-router
        consumes: trigger
        produces: llm.response
        config:
          strategy: best-first
      - plugin: event-emitter
        consumes: llm.response
        produces:
          - task.created
          - brain.fact.upsert
  background:
    - plugin: entity-extractor
      consumes: brain.content.stored
      produces: brain.entity.upsert
  rate_limit:
    max_calls_per_minute: 30
    max_daily_spend_usd: 5.00
"#;
        let file: AgentConfigFile = serde_yaml::from_str(yaml).unwrap();
        let agent = &file.agent;
        assert_eq!(agent.id, "personal-assistant");
        assert_eq!(agent.name, "Personal Assistant");
        assert!(agent.enabled);
        assert_eq!(agent.scope, "scope_root");
        assert_eq!(agent.additional_scopes.len(), 1);
        assert_eq!(agent.pipeline.triggers.len(), 1);
        assert_eq!(agent.pipeline.steps.len(), 2);

        // Check StringOrVec::Single
        assert_eq!(agent.pipeline.steps[0].produces.as_vec(), vec!["llm.response"]);

        // Check StringOrVec::Multiple
        assert_eq!(
            agent.pipeline.steps[1].produces.as_vec(),
            vec!["task.created", "brain.fact.upsert"]
        );

        assert!(agent.background.is_some());
        assert_eq!(agent.background.as_ref().unwrap().len(), 1);

        let rate = agent.rate_limit.as_ref().unwrap();
        assert_eq!(rate.max_calls_per_minute, 30);
        assert!((rate.max_daily_spend_usd - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_minimal_agent_config() {
        let yaml = r#"
agent:
  id: simple-bot
  name: Simple Bot
  enabled: true
  scope: scope_root
  pipeline:
    triggers:
      - channel.cli.inbound
    steps:
      - plugin: llm-router
        consumes: trigger
        produces: llm.response
      - plugin: response-formatter
        consumes: llm.response
        produces: channel.cli.outbound
"#;
        let file: AgentConfigFile = serde_yaml::from_str(yaml).unwrap();
        let agent = &file.agent;
        assert_eq!(agent.id, "simple-bot");
        assert!(agent.description.is_none());
        assert!(agent.additional_scopes.is_empty());
        assert!(agent.background.is_none());
        assert!(agent.rate_limit.is_none());
    }
}
