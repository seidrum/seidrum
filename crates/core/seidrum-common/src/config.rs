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
    /// Eventbus WebSocket URL for remote plugins.
    /// Accepts the old `nats_url` key for backwards compatibility.
    #[serde(alias = "nats_url")]
    pub eventbus_url: String,

    /// ArangoDB server URL.
    pub arango_url: String,

    /// ArangoDB database name.
    #[serde(default = "default_arango_database")]
    pub arango_database: String,

    /// Directory containing agent YAML definitions.
    #[serde(default = "default_agents_dir")]
    pub agents_dir: String,

    /// Directory containing workflow YAML definitions.
    #[serde(default = "default_workflows_dir")]
    pub workflows_dir: String,

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

fn default_workflows_dir() -> String {
    "workflows/".to_string()
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
    /// Bus subjects that start this pipeline (supports wildcards).
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
    let contents = std::fs::read_to_string(path).map_err(|e| {
        anyhow::anyhow!(
            "Failed to read platform config at {}: {}",
            path.display(),
            e
        )
    })?;
    let config: PlatformConfig = serde_yaml::from_str(&contents).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse platform config at {}: {}",
            path.display(),
            e
        )
    })?;
    Ok(config)
}

/// Load and parse an agent configuration from a YAML file.
pub fn load_agent_config(path: &Path) -> anyhow::Result<AgentConfigFile> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read agent config at {}: {}", path.display(), e))?;
    let config: AgentConfigFile = serde_yaml::from_str(&contents).map_err(|e| {
        anyhow::anyhow!("Failed to parse agent config at {}: {}", path.display(), e)
    })?;
    Ok(config)
}

// ---------------------------------------------------------------------------
// V2 Agent definition (simplified: prompt + tools + scope)
// ---------------------------------------------------------------------------

/// V2 agent config: just prompt + tools + scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinitionFile {
    pub agent: AgentDefinition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub tools: Vec<String>,
    pub scope: String,
    #[serde(default)]
    pub additional_scopes: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Whether this agent is enabled. Defaults to false so first boot starts clean.
    #[serde(default)]
    pub enabled: bool,
    /// Bus subjects this agent subscribes to for consciousness events.
    #[serde(default)]
    pub subscribe: Vec<String>,
    /// Per-agent guardrail overrides.
    #[serde(default)]
    pub guardrails: Option<GuardrailOverrides>,
}

/// Per-agent guardrail overrides. Fields not set use defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardrailOverrides {
    pub turn_limit: Option<u32>,
    pub time_limit_seconds: Option<u64>,
    pub hitl_after_turns: Option<u32>,
}

pub fn load_agent_definition(path: &Path) -> anyhow::Result<AgentDefinitionFile> {
    let contents = std::fs::read_to_string(path)?;
    let def: AgentDefinitionFile = serde_yaml::from_str(&contents)?;
    Ok(def)
}

// ---------------------------------------------------------------------------
// Workflow configuration (workflows/*.yaml)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowFile {
    pub workflow: WorkflowConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowConfig {
    pub id: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub agents: HashMap<String, WorkflowAgentRef>,
    pub on: String,
    pub steps: Vec<WorkflowStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowAgentRef {
    #[serde(default)]
    pub ref_id: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WorkflowStep {
    Plugin { plugin: String },
    Agent { agent: String },
    Output { output: WorkflowOutput },
    Condition { condition: WorkflowCondition },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WorkflowOutput {
    Simple(String),
    Detailed {
        channel: String,
        chat_id: Option<String>,
        template: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowCondition {
    #[serde(rename = "if")]
    pub condition_if: String,
    pub then_step: Option<String>,
    pub else_step: Option<String>,
}

pub fn load_workflow(path: &Path) -> anyhow::Result<WorkflowFile> {
    let contents = std::fs::read_to_string(path)?;
    let wf: WorkflowFile = serde_yaml::from_str(&contents)?;
    Ok(wf)
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
eventbus_url: ws://localhost:9000
arango_url: http://localhost:8529
"#;
        let config: PlatformConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.eventbus_url, "ws://localhost:9000");
        assert_eq!(config.arango_url, "http://localhost:8529");
        assert_eq!(config.arango_database, "seidrum");
        assert_eq!(config.agents_dir, "agents/");
        assert_eq!(config.log_level, "info");
    }

    #[test]
    fn test_platform_config_overrides() {
        let yaml = r#"
eventbus_url: ws://kernel:9000
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
        assert_eq!(
            agent.pipeline.steps[0].produces.as_vec(),
            vec!["llm.response"]
        );

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
    fn test_platform_config_workflows_dir_default() {
        let yaml = r#"
eventbus_url: ws://localhost:9000
arango_url: http://localhost:8529
"#;
        let config: PlatformConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.workflows_dir, "workflows/");
    }

    #[test]
    fn test_agent_definition_roundtrip() {
        let yaml = r#"
agent:
  id: personal-assistant
  description: General-purpose personal assistant
  prompt: ./prompts/assistant.md
  tools:
    - brain-query
    - execute-code
  scope: scope_root
  additional_scopes:
    - scope_job_search
"#;
        let file: AgentDefinitionFile = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(file.agent.id, "personal-assistant");
        assert_eq!(file.agent.prompt, "./prompts/assistant.md");
        assert_eq!(file.agent.tools, vec!["brain-query", "execute-code"]);
        assert_eq!(file.agent.scope, "scope_root");
        assert_eq!(file.agent.additional_scopes, vec!["scope_job_search"]);
        assert_eq!(
            file.agent.description.as_deref(),
            Some("General-purpose personal assistant")
        );

        let json = serde_json::to_string(&file).unwrap();
        let deserialized: AgentDefinitionFile = serde_json::from_str(&json).unwrap();
        assert_eq!(file.agent.id, deserialized.agent.id);
        assert_eq!(file.agent.tools, deserialized.agent.tools);
    }

    #[test]
    fn test_workflow_config_roundtrip_plugin_and_agent_steps() {
        let yaml = r#"
workflow:
  id: default
  description: Default conversational workflow
  agents:
    assistant:
      ref_id: personal-assistant
  on: "channel.*.inbound"
  steps:
    - plugin: content-ingester
    - agent: assistant
    - output: origin
"#;
        let file: WorkflowFile = serde_yaml::from_str(yaml).unwrap();
        let wf = &file.workflow;
        assert_eq!(wf.id, "default");
        assert_eq!(wf.on, "channel.*.inbound");
        assert_eq!(wf.steps.len(), 3);
        assert!(wf.agents.contains_key("assistant"));
        assert_eq!(
            wf.agents["assistant"].ref_id.as_deref(),
            Some("personal-assistant")
        );

        let json = serde_json::to_string(&file).unwrap();
        let deserialized: WorkflowFile = serde_json::from_str(&json).unwrap();
        assert_eq!(wf.id, deserialized.workflow.id);
        assert_eq!(wf.steps.len(), deserialized.workflow.steps.len());
    }

    #[test]
    fn test_workflow_config_with_condition_step() {
        let yaml = r#"
workflow:
  id: conditional
  on: "channel.*.inbound"
  steps:
    - plugin: content-ingester
    - condition:
        if: "payload.text starts_with '/'"
        then_step: command-handler
        else_step: assistant
"#;
        let file: WorkflowFile = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(file.workflow.steps.len(), 2);
    }

    #[test]
    fn test_workflow_output_detailed() {
        let yaml = r#"
workflow:
  id: output-test
  on: "channel.*.inbound"
  steps:
    - output:
        channel: telegram
        chat_id: "12345"
        template: response.md
"#;
        let file: WorkflowFile = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(file.workflow.steps.len(), 1);
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

// ---------------------------------------------------------------------------
// Preset definitions
// ---------------------------------------------------------------------------

/// Top-level wrapper for preset YAML files (config/presets/*.yaml)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetFile {
    pub preset: Preset,
}

/// Preset configuration that defines plugin + agent requirements and dependencies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    /// Unique identifier (e.g., "chat-bot", "developer", "api-service")
    pub id: String,

    /// Human-readable name (e.g., "Chat Bot", "Developer Assistant")
    pub name: String,

    /// Description of what this preset provides
    pub description: String,

    /// Icon name (e.g., "message-circle", "code", "globe")
    #[serde(default)]
    pub icon: String,

    /// Plugin requirements (required + recommended)
    pub plugins: PresetPlugins,

    /// Agent requirements (required + recommended)
    pub agents: PresetAgents,

    /// Environment variables needed to enable this preset
    #[serde(default)]
    pub env_required: Vec<EnvRequirement>,

    /// Preferred LLM provider for this preset
    #[serde(default)]
    pub llm_provider: Option<String>,

    /// Semantic version of this preset
    #[serde(default)]
    pub version: Option<String>,

    /// Author of this preset
    #[serde(default)]
    pub author: Option<String>,

    /// Repository URL for this preset
    #[serde(default)]
    pub repository: Option<String>,

    /// Bundle contents — files included in this preset package
    #[serde(default)]
    pub bundle: Option<PresetBundle>,
}

/// Plugin list for a preset (required + recommended)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetPlugins {
    /// Plugins that must be enabled for this preset to function
    pub required: Vec<String>,

    /// Plugins that enhance the preset but are optional
    #[serde(default)]
    pub recommended: Vec<String>,
}

/// Agent list for a preset (required + recommended)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetAgents {
    /// Agents that must be enabled for this preset to function
    pub required: Vec<String>,

    /// Agents that enhance the preset but are optional
    #[serde(default)]
    pub recommended: Vec<String>,
}

/// Environment variable requirement for a preset
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvRequirement {
    /// Environment variable name
    pub key: String,

    /// Human-readable label for UI
    pub label: String,

    /// Help text explaining what this variable is for
    #[serde(default)]
    pub help: String,

    /// Auto-generation strategy (e.g., "hex:32" for random hex string)
    #[serde(default)]
    pub auto_generate: Option<String>,
}

/// Bundle contents declaration for a preset
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetBundle {
    /// Agent YAML files included in the bundle (relative paths)
    #[serde(default)]
    pub agents: Vec<String>,

    /// Prompt files included in the bundle (relative paths)
    #[serde(default)]
    pub prompts: Vec<String>,

    /// New plugin entries to add to plugins.yaml
    #[serde(default)]
    pub plugins: Vec<BundledPlugin>,
}

/// A plugin entry to be added to plugins.yaml when a bundle is imported
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundledPlugin {
    /// Plugin name/identifier
    pub name: String,

    /// Binary name for this plugin
    pub binary: String,

    /// Environment variables for this plugin
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

/// Load and parse a preset configuration from a YAML file.
pub fn load_preset(path: &Path) -> anyhow::Result<PresetFile> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read preset at {}: {}", path.display(), e))?;
    let preset: PresetFile = serde_yaml::from_str(&contents)
        .map_err(|e| anyhow::anyhow!("Failed to parse preset at {}: {}", path.display(), e))?;
    Ok(preset)
}

// ---------------------------------------------------------------------------
// Package manager types
// ---------------------------------------------------------------------------

/// The kind of package.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PackageKind {
    Plugin,
    Agent,
    Bundle,
}

/// Top-level seidrum-pkg.yaml
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageManifestFile {
    pub package: PackageMeta,
    #[serde(default)]
    pub plugin: Option<PluginPackageSpec>,
    #[serde(default)]
    pub agent: Option<AgentPackageSpec>,
    #[serde(default)]
    pub bundle: Option<BundleSpec>,
}

/// Package metadata (common to all kinds).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageMeta {
    pub name: String,
    pub version: String,
    pub kind: PackageKind,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub min_seidrum_version: Option<String>,
}

/// Plugin-specific package spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginPackageSpec {
    pub binary: String,
    #[serde(default)]
    pub consumes: Vec<String>,
    #[serde(default)]
    pub produces: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<PackageCapability>,
    #[serde(default)]
    pub env: Vec<EnvRequirement>,
    #[serde(default)]
    pub artifacts: Vec<PackageArtifact>,
}

/// A capability (tool/command) declared by a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageCapability {
    pub name: String,
    #[serde(default = "default_capability_kind")]
    pub kind: String,
    #[serde(default)]
    pub description: Option<String>,
}

fn default_capability_kind() -> String {
    "tool".to_string()
}

/// A pre-built binary artifact for a specific platform.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageArtifact {
    pub target: String,
    pub url: String,
    #[serde(default)]
    pub image: Option<String>,
    pub sha256: String,
}

/// Agent-specific package spec (used when kind=agent or inside bundles).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPackageSpec {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub subscribe: Vec<String>,
}

/// Bundle spec — contains plugins, agents, prompts, and a preset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleSpec {
    #[serde(default)]
    pub preset: Option<BundlePreset>,
    #[serde(default)]
    pub includes: Vec<BundleDependency>,
    #[serde(default)]
    pub agents: Vec<AgentPackageSpec>,
    #[serde(default)]
    pub prompts: Vec<String>,
}

/// Preset embedded in a bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundlePreset {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub plugins: Option<PresetPlugins>,
    #[serde(default)]
    pub agents: Option<PresetAgents>,
    #[serde(default)]
    pub env_required: Vec<EnvRequirement>,
}

/// A dependency reference in a bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleDependency {
    #[serde(default)]
    pub package: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
}

// Registry index types

/// The top-level registry index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryIndex {
    #[serde(default)]
    pub packages: Vec<RegistryEntry>,
}

/// A single entry in the registry index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub name: String,
    pub latest: String,
    pub kind: PackageKind,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub downloads: Option<u64>,
}

/// Installed package tracking (stored in installed.yaml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPackages {
    #[serde(default)]
    pub packages: Vec<InstalledPackage>,
}

/// A single installed package record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPackage {
    pub name: String,
    pub version: String,
    pub kind: PackageKind,
    pub source: String,
    #[serde(default)]
    pub installed_at: Option<String>,
    #[serde(default)]
    pub files: Vec<String>,
}

pub fn load_package_manifest(path: &Path) -> anyhow::Result<PackageManifestFile> {
    let contents = std::fs::read_to_string(path)?;
    let manifest: PackageManifestFile = serde_yaml::from_str(&contents)?;
    Ok(manifest)
}

pub fn load_registry_index(path: &Path) -> anyhow::Result<RegistryIndex> {
    let contents = std::fs::read_to_string(path)?;
    let index: RegistryIndex = serde_yaml::from_str(&contents)?;
    Ok(index)
}

pub fn load_installed_packages(path: &Path) -> anyhow::Result<InstalledPackages> {
    if !path.exists() {
        return Ok(InstalledPackages { packages: vec![] });
    }
    let contents = std::fs::read_to_string(path)?;
    let installed: InstalledPackages = serde_yaml::from_str(&contents)?;
    Ok(installed)
}

pub fn save_installed_packages(path: &Path, installed: &InstalledPackages) -> anyhow::Result<()> {
    let contents = serde_yaml::to_string(installed)?;
    std::fs::write(path, contents)?;
    Ok(())
}
