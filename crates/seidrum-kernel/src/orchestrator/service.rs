//! Workflow engine — replaces the linear agent orchestrator.
//!
//! Loads workflow YAML definitions from the workflows/ directory and agent
//! definitions from the agents/ directory. Subscribes to trigger events defined
//! in each workflow's `on` field, injects origin metadata, and handles response
//! routing based on workflow output steps.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures::StreamExt;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use seidrum_common::config::{
    load_agent_config, load_agent_definition, load_workflow, AgentConfig, AgentDefinition,
    WorkflowConfig, WorkflowOutput, WorkflowStep,
};
use seidrum_common::events::{ChannelInbound, EventEnvelope, EventOrigin};

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A workflow loaded from YAML with its resolved agent references.
#[derive(Debug, Clone)]
struct LoadedWorkflow {
    config: WorkflowConfig,
    /// Agent name (as used in workflow steps) -> full AgentDefinition.
    resolved_agents: HashMap<String, AgentDefinition>,
}

/// Thread-safe workflow engine.
#[derive(Clone)]
pub struct WorkflowEngine {
    /// Loaded workflows keyed by workflow ID.
    workflows: Arc<RwLock<Vec<LoadedWorkflow>>>,
    /// All loaded agent definitions keyed by agent ID.
    agents: Arc<RwLock<HashMap<String, AgentDefinition>>>,
    /// Tracks origin per correlation_id / event_id for response routing.
    origins: Arc<RwLock<HashMap<String, EventOrigin>>>,
    /// Legacy V1 agent configs (for backward compat / validate).
    legacy_agents: Arc<RwLock<HashMap<String, LegacyLoadedAgent>>>,
}

/// Kept for backward compatibility with V1 agent pipeline validation.
#[derive(Debug, Clone)]
struct LegacyLoadedAgent {
    config: AgentConfig,
    #[allow(dead_code)]
    warnings: Vec<String>,
}

impl WorkflowEngine {
    /// Create a new, empty workflow engine.
    pub fn new() -> Self {
        Self {
            workflows: Arc::new(RwLock::new(Vec::new())),
            agents: Arc::new(RwLock::new(HashMap::new())),
            origins: Arc::new(RwLock::new(HashMap::new())),
            legacy_agents: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    // -----------------------------------------------------------------------
    // Loading
    // -----------------------------------------------------------------------

    /// Load all agent definitions (V2 format) from the agents directory.
    /// Returns the number of agents loaded.
    pub async fn load_agents(&self, agents_dir: &str) -> Result<usize> {
        let agents_path = Path::new(agents_dir);
        if !agents_path.is_dir() {
            return Err(anyhow::anyhow!(
                "agents directory not found: {}",
                agents_path.display()
            ));
        }

        let entries = std::fs::read_dir(agents_path)
            .with_context(|| format!("cannot read agents directory: {}", agents_path.display()))?;

        let mut loaded_count = 0;

        for entry in entries {
            let entry = entry.context("error reading directory entry")?;
            let path = entry.path();

            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext != "yaml" && ext != "yml" {
                continue;
            }

            // Try V2 format first (prompt + tools + scope)
            if let Ok(def_file) = load_agent_definition(&path) {
                let def = def_file.agent;
                info!(
                    agent_id = %def.id,
                    scope = %def.scope,
                    tools = ?def.tools,
                    "agent loaded (V2 format)"
                );

                let mut agents = self.agents.write().await;
                agents.insert(def.id.clone(), def);
                loaded_count += 1;
                continue;
            }

            // Fall back to V1 format (pipeline-based)
            match load_agent_config(&path) {
                Ok(agent_file) => {
                    let agent = agent_file.agent;
                    if !agent.enabled {
                        info!(agent_id = %agent.id, "agent is disabled, skipping");
                        continue;
                    }

                    let warnings = self.validate_pipeline(&agent);
                    for w in &warnings {
                        warn!(agent_id = %agent.id, warning = %w, "pipeline validation warning");
                    }

                    info!(
                        agent_id = %agent.id,
                        agent_name = %agent.name,
                        triggers = ?agent.pipeline.triggers,
                        steps = agent.pipeline.steps.len(),
                        "agent loaded (V1 pipeline format)"
                    );

                    let mut legacy = self.legacy_agents.write().await;
                    legacy.insert(
                        agent.id.clone(),
                        LegacyLoadedAgent {
                            config: agent,
                            warnings,
                        },
                    );
                    loaded_count += 1;
                }
                Err(e) => {
                    error!(
                        path = %path.display(),
                        error = %e,
                        "failed to load agent config"
                    );
                }
            }
        }

        Ok(loaded_count)
    }

    /// Load all workflow definitions from the workflows directory.
    /// Agent definitions must be loaded first so references can be resolved.
    /// Returns the number of workflows loaded.
    pub async fn load_workflows(&self, workflows_dir: &str) -> Result<usize> {
        let wf_path = Path::new(workflows_dir);
        if !wf_path.is_dir() {
            return Err(anyhow::anyhow!(
                "workflows directory not found: {}",
                wf_path.display()
            ));
        }

        let entries = std::fs::read_dir(wf_path)
            .with_context(|| format!("cannot read workflows directory: {}", wf_path.display()))?;

        let agents = self.agents.read().await;
        let mut loaded_count = 0;

        for entry in entries {
            let entry = entry.context("error reading directory entry")?;
            let path = entry.path();

            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext != "yaml" && ext != "yml" {
                continue;
            }

            match load_workflow(&path) {
                Ok(wf_file) => {
                    let config = wf_file.workflow;

                    // Resolve agent references declared in the workflow.
                    let mut resolved_agents = HashMap::new();
                    for (name, agent_ref) in &config.agents {
                        // The ref_id points to an agent definition by ID.
                        // If no ref_id, the workflow defines the agent inline
                        // (prompt/tools/scope directly).
                        if let Some(ref_id) = &agent_ref.ref_id {
                            if let Some(def) = agents.get(ref_id) {
                                resolved_agents.insert(name.clone(), def.clone());
                            } else {
                                warn!(
                                    workflow_id = %config.id,
                                    agent_name = %name,
                                    ref_id = %ref_id,
                                    "workflow references unknown agent definition"
                                );
                            }
                        } else {
                            // Inline agent definition within the workflow.
                            let inline_def = AgentDefinition {
                                id: name.clone(),
                                prompt: agent_ref.prompt.clone().unwrap_or_default(),
                                tools: agent_ref.tools.clone(),
                                scope: agent_ref
                                    .scope
                                    .clone()
                                    .unwrap_or_else(|| "scope_root".to_string()),
                                additional_scopes: vec![],
                                description: None,
                            };
                            resolved_agents.insert(name.clone(), inline_def);
                        }
                    }

                    info!(
                        workflow_id = %config.id,
                        trigger = %config.on,
                        steps = config.steps.len(),
                        resolved_agents = resolved_agents.len(),
                        "workflow loaded"
                    );

                    let mut workflows = self.workflows.write().await;
                    workflows.push(LoadedWorkflow {
                        config,
                        resolved_agents,
                    });
                    loaded_count += 1;
                }
                Err(e) => {
                    error!(
                        path = %path.display(),
                        error = %e,
                        "failed to load workflow"
                    );
                }
            }
        }

        Ok(loaded_count)
    }

    // -----------------------------------------------------------------------
    // Spawning
    // -----------------------------------------------------------------------

    /// Spawn the workflow engine: load agents + workflows, set up NATS
    /// subscriptions for trigger events, and handle response routing.
    pub async fn spawn(
        self,
        nats_client: async_nats::Client,
        agents_dir: &str,
        workflows_dir: &str,
    ) -> Result<tokio::task::JoinHandle<()>> {
        // Load agents first (workflows reference them).
        let agent_count = self.load_agents(agents_dir).await?;
        info!(count = agent_count, "workflow engine loaded agents");

        // Load workflows.
        let wf_count = self.load_workflows(workflows_dir).await?;
        info!(count = wf_count, "workflow engine loaded workflows");

        if wf_count == 0 {
            warn!("no workflows found — workflow engine will idle");
        }

        // Collect trigger subjects and their associated workflow scopes.
        let workflows = self.workflows.read().await;
        let mut trigger_infos: HashMap<String, Vec<TriggerContext>> = HashMap::new();

        for loaded_wf in workflows.iter() {
            let trigger_subject = &loaded_wf.config.on;

            // Determine scope from the first resolved agent (if any).
            let scope = loaded_wf
                .resolved_agents
                .values()
                .next()
                .map(|a| a.scope.clone())
                .unwrap_or_else(|| "scope_root".to_string());

            let ctx = TriggerContext {
                workflow_id: loaded_wf.config.id.clone(),
                scope,
                output_steps: loaded_wf
                    .config
                    .steps
                    .iter()
                    .filter_map(|s| match s {
                        WorkflowStep::Output { output } => Some(output.clone()),
                        _ => None,
                    })
                    .collect(),
            };

            trigger_infos
                .entry(trigger_subject.clone())
                .or_default()
                .push(ctx);
        }
        drop(workflows);

        // Subscribe to each unique trigger subject.
        let mut trigger_subs = Vec::new();
        for (subject, contexts) in &trigger_infos {
            let sub = nats_client
                .subscribe(subject.clone())
                .await
                .with_context(|| format!("failed to subscribe to trigger subject: {}", subject))?;
            info!(subject = %subject, workflows = contexts.len(), "subscribed to trigger");
            trigger_subs.push((sub, contexts.clone()));
        }

        // Subscribe to llm.response for response routing on non-origin outputs.
        let llm_response_sub = nats_client
            .subscribe("llm.response".to_string())
            .await
            .with_context(|| "failed to subscribe to llm.response")?;
        info!("subscribed to llm.response for response routing");

        let nats = nats_client.clone();
        let engine = self.clone();

        let handle = tokio::spawn(async move {
            let mut handles = Vec::new();

            // Spawn a task per trigger subscription.
            for (mut sub, contexts) in trigger_subs {
                let nats = nats.clone();
                let engine = engine.clone();

                let h = tokio::spawn(async move {
                    while let Some(msg) = sub.next().await {
                        // Parse the incoming event envelope.
                        let mut envelope: EventEnvelope = match serde_json::from_slice(&msg.payload)
                        {
                            Ok(env) => env,
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    subject = %msg.subject,
                                    "failed to parse trigger event as EventEnvelope"
                                );
                                continue;
                            }
                        };

                        // Inject origin from ChannelInbound payload if not already set.
                        if envelope.origin.is_none() {
                            if let Ok(inbound) =
                                serde_json::from_value::<ChannelInbound>(envelope.payload.clone())
                            {
                                let thread_id = inbound.metadata.get("thread_id").cloned();
                                let message_id = inbound.metadata.get("message_id").cloned();
                                envelope.origin = Some(EventOrigin {
                                    platform: inbound.platform,
                                    chat_id: inbound.chat_id,
                                    thread_id,
                                    message_id,
                                });
                            }
                        }

                        // Store origin for response routing (keyed by correlation_id
                        // or event ID as fallback).
                        if let Some(ref origin) = envelope.origin {
                            let key = envelope
                                .correlation_id
                                .clone()
                                .unwrap_or_else(|| envelope.id.clone());
                            let mut origins = engine.origins.write().await;
                            origins.insert(key, origin.clone());
                        }

                        // For each workflow triggered by this subject, inject scope
                        // and republish so pipeline plugins pick it up.
                        for ctx in &contexts {
                            if envelope.scope.is_none() {
                                envelope.scope = Some(ctx.scope.clone());
                            }

                            let pipeline_subject = format!(
                                "agent.{}.pipeline.{}",
                                ctx.workflow_id, envelope.event_type
                            );

                            match serde_json::to_vec(&envelope) {
                                Ok(bytes) => {
                                    if let Err(e) =
                                        nats.publish(pipeline_subject.clone(), bytes.into()).await
                                    {
                                        error!(
                                            error = %e,
                                            subject = %pipeline_subject,
                                            "failed to publish scope-injected event"
                                        );
                                    } else {
                                        info!(
                                            event_id = %envelope.id,
                                            workflow_id = %ctx.workflow_id,
                                            scope = %ctx.scope,
                                            original_subject = %envelope.event_type,
                                            "forwarded trigger event with scope"
                                        );
                                    }
                                }
                                Err(e) => {
                                    error!(
                                        error = %e,
                                        "failed to serialize scope-injected event"
                                    );
                                }
                            }
                        }
                    }
                });
                handles.push(h);
            }

            // Spawn llm.response handler for non-origin routing.
            {
                let engine = engine.clone();
                let nats = nats.clone();

                let h = tokio::spawn(async move {
                    let mut sub = llm_response_sub;
                    while let Some(msg) = sub.next().await {
                        let envelope: EventEnvelope = match serde_json::from_slice(&msg.payload) {
                            Ok(env) => env,
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    "failed to parse llm.response as EventEnvelope"
                                );
                                continue;
                            }
                        };

                        // Check if the envelope already has origin — the
                        // response-formatter handles those cases. We only
                        // intervene for workflows with explicit non-origin
                        // output routing.
                        if envelope.origin.is_some() {
                            // Response-formatter already handles origin-based
                            // routing. Nothing extra to do.
                            continue;
                        }

                        // Try to find the origin from our stored map.
                        let key = envelope
                            .correlation_id
                            .clone()
                            .unwrap_or_else(|| envelope.id.clone());

                        let origin = {
                            let mut origins = engine.origins.write().await;
                            origins.remove(&key)
                        };

                        if let Some(origin) = origin {
                            // Re-publish the llm.response with origin injected
                            // so the response-formatter can route it.
                            let mut enriched = envelope.clone();
                            enriched.origin = Some(origin.clone());

                            let outbound_subject = format!("channel.{}.outbound", origin.platform);

                            match serde_json::to_vec(&enriched) {
                                Ok(bytes) => {
                                    if let Err(e) =
                                        nats.publish(outbound_subject.clone(), bytes.into()).await
                                    {
                                        error!(
                                            error = %e,
                                            subject = %outbound_subject,
                                            "failed to route llm.response to outbound"
                                        );
                                    } else {
                                        info!(
                                            correlation_id = %key,
                                            platform = %origin.platform,
                                            chat_id = %origin.chat_id,
                                            "routed orphaned llm.response via stored origin"
                                        );
                                    }
                                }
                                Err(e) => {
                                    error!(
                                        error = %e,
                                        "failed to serialize enriched llm.response"
                                    );
                                }
                            }
                        }
                    }
                });
                handles.push(h);
            }

            // Wait for all handlers.
            for h in handles {
                if let Err(e) = h.await {
                    error!(error = %e, "workflow engine handler panicked");
                }
            }

            info!("workflow engine stopped");
        });

        Ok(handle)
    }

    // -----------------------------------------------------------------------
    // Query methods
    // -----------------------------------------------------------------------

    /// Get an agent definition by ID (V2 format).
    pub async fn get_agent(&self, agent_id: &str) -> Option<AgentDefinition> {
        let agents = self.agents.read().await;
        agents.get(agent_id).cloned()
    }

    /// Get all loaded agent definitions.
    pub async fn list_agents(&self) -> Vec<AgentDefinition> {
        let agents = self.agents.read().await;
        agents.values().cloned().collect()
    }

    /// Get all loaded workflow configs.
    pub async fn list_workflows(&self) -> Vec<WorkflowConfig> {
        let workflows = self.workflows.read().await;
        workflows.iter().map(|lw| lw.config.clone()).collect()
    }

    // -----------------------------------------------------------------------
    // V1 pipeline validation (backward compat)
    // -----------------------------------------------------------------------

    /// Validate that a V1 pipeline's event type chains are coherent.
    pub fn validate_pipeline(&self, agent: &AgentConfig) -> Vec<String> {
        let mut warnings = Vec::new();
        let steps = &agent.pipeline.steps;

        if steps.is_empty() {
            warnings.push("pipeline has no steps".to_string());
            return warnings;
        }

        if agent.pipeline.triggers.is_empty() {
            warnings.push("pipeline has no triggers".to_string());
        }

        let mut available_events: Vec<String> = vec!["trigger".to_string()];
        for trigger in &agent.pipeline.triggers {
            available_events.push(trigger.clone());
        }

        for (i, step) in steps.iter().enumerate() {
            let consumes = &step.consumes;

            if !Self::event_matches_any(consumes, &available_events) {
                warnings.push(format!(
                    "step {} (plugin '{}') consumes '{}' but no previous step produces it",
                    i, step.plugin, consumes
                ));
            }

            for produced in step.produces.as_vec() {
                available_events.push(produced.to_string());
            }
        }

        if let Some(bg_steps) = &agent.background {
            for bg in bg_steps {
                if bg.consumes == "trigger" {
                    warnings.push(format!(
                        "background step (plugin '{}') consumes 'trigger' — \
                         background steps typically consume brain/agent events",
                        bg.plugin
                    ));
                }
            }
        }

        warnings
    }

    /// Check if an event type matches any in the available set.
    fn event_matches_any(event_type: &str, available: &[String]) -> bool {
        for available_event in available {
            if Self::event_pattern_matches(event_type, available_event)
                || Self::event_pattern_matches(available_event, event_type)
            {
                return true;
            }
        }
        false
    }

    /// Simple wildcard pattern matching for NATS-style subjects.
    fn event_pattern_matches(subject: &str, pattern: &str) -> bool {
        if subject == pattern {
            return true;
        }

        let subject_parts: Vec<&str> = subject.split('.').collect();
        let pattern_parts: Vec<&str> = pattern.split('.').collect();

        // Handle `>` at the end of pattern (matches rest)
        if pattern_parts.last() == Some(&">") {
            if subject_parts.len() < pattern_parts.len() - 1 {
                return false;
            }
            for (s, p) in subject_parts.iter().zip(pattern_parts.iter()) {
                if *p == ">" {
                    return true;
                }
                if *p != "*" && s != p {
                    return false;
                }
            }
            return true;
        }

        if subject_parts.len() != pattern_parts.len() {
            return false;
        }

        for (s, p) in subject_parts.iter().zip(pattern_parts.iter()) {
            if *p != "*" && s != p {
                return false;
            }
        }

        true
    }
}

/// Context associated with a trigger subscription for a specific workflow.
#[derive(Debug, Clone)]
struct TriggerContext {
    workflow_id: String,
    scope: String,
    /// Output steps from the workflow (for routing decisions).
    #[allow(dead_code)]
    output_steps: Vec<WorkflowOutput>,
}

// ---------------------------------------------------------------------------
// Backward-compat re-export: main.rs uses OrchestratorService
// ---------------------------------------------------------------------------

/// Type alias for backward compatibility.
pub type OrchestratorService = WorkflowEngine;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use seidrum_common::config::{AgentConfig, Pipeline, PipelineStep, StringOrVec};

    fn make_step(plugin: &str, consumes: &str, produces: &str) -> PipelineStep {
        PipelineStep {
            plugin: plugin.to_string(),
            consumes: consumes.to_string(),
            produces: StringOrVec::Single(produces.to_string()),
            config: None,
        }
    }

    fn make_agent(steps: Vec<PipelineStep>, triggers: Vec<&str>) -> AgentConfig {
        AgentConfig {
            id: "test-agent".to_string(),
            name: "Test Agent".to_string(),
            description: None,
            enabled: true,
            scope: "scope_root".to_string(),
            additional_scopes: vec![],
            pipeline: Pipeline {
                triggers: triggers.into_iter().map(String::from).collect(),
                steps,
            },
            background: None,
            rate_limit: None,
        }
    }

    #[test]
    fn valid_pipeline_no_warnings() {
        let engine = WorkflowEngine::new();
        let agent = make_agent(
            vec![
                make_step("content-ingester", "trigger", "brain.content.stored"),
                make_step("llm-router", "brain.content.stored", "llm.response"),
                make_step("response-formatter", "llm.response", "channel.cli.outbound"),
            ],
            vec!["channel.cli.inbound"],
        );
        let warnings = engine.validate_pipeline(&agent);
        assert!(
            warnings.is_empty(),
            "expected no warnings, got: {:?}",
            warnings
        );
    }

    #[test]
    fn pipeline_broken_chain_produces_warning() {
        let engine = WorkflowEngine::new();
        let agent = make_agent(
            vec![
                make_step("content-ingester", "trigger", "brain.content.stored"),
                make_step("llm-router", "agent.context.loaded", "llm.response"),
            ],
            vec!["channel.cli.inbound"],
        );
        let warnings = engine.validate_pipeline(&agent);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("agent.context.loaded"));
    }

    #[test]
    fn trigger_consumes_is_always_valid() {
        let engine = WorkflowEngine::new();
        let agent = make_agent(
            vec![make_step(
                "graph-context-loader",
                "trigger",
                "agent.context.loaded",
            )],
            vec!["channel.cli.inbound"],
        );
        let warnings = engine.validate_pipeline(&agent);
        assert!(warnings.is_empty());
    }

    #[test]
    fn empty_pipeline_produces_warning() {
        let engine = WorkflowEngine::new();
        let agent = make_agent(vec![], vec!["channel.cli.inbound"]);
        let warnings = engine.validate_pipeline(&agent);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("no steps"));
    }

    #[test]
    fn no_triggers_produces_warning() {
        let engine = WorkflowEngine::new();
        let agent = make_agent(
            vec![make_step("llm-router", "trigger", "llm.response")],
            vec![],
        );
        let warnings = engine.validate_pipeline(&agent);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("no triggers"));
    }

    #[test]
    fn wildcard_pattern_matching() {
        assert!(WorkflowEngine::event_pattern_matches(
            "channel.cli.outbound",
            "channel.*.outbound"
        ));
        assert!(!WorkflowEngine::event_pattern_matches(
            "channel.cli.inbound",
            "channel.*.outbound"
        ));

        assert!(WorkflowEngine::event_pattern_matches(
            "llm.response",
            "llm.response"
        ));

        assert!(WorkflowEngine::event_pattern_matches(
            "task.completed.abc123",
            "task.completed.>"
        ));
        assert!(WorkflowEngine::event_pattern_matches(
            "task.completed.abc.def",
            "task.completed.>"
        ));

        assert!(!WorkflowEngine::event_pattern_matches("a.b.c", "a.b"));
    }

    #[test]
    fn wildcard_produces_matches_consumes() {
        let engine = WorkflowEngine::new();
        let agent = make_agent(
            vec![
                make_step("llm-router", "trigger", "llm.response"),
                make_step("response-formatter", "llm.response", "channel.*.outbound"),
            ],
            vec!["channel.cli.inbound"],
        );
        let warnings = engine.validate_pipeline(&agent);
        assert!(
            warnings.is_empty(),
            "expected no warnings, got: {:?}",
            warnings
        );
    }

    #[tokio::test]
    async fn load_agents_from_directory() {
        let engine = WorkflowEngine::new();
        let agents_dir = format!("{}/../../agents/", env!("CARGO_MANIFEST_DIR"));
        let result = engine.load_agents(&agents_dir).await;
        assert!(result.is_ok());
        let count = result.unwrap();
        assert_eq!(count, 2, "Should load 2 V2 agent files");
    }

    #[tokio::test]
    async fn load_workflows_from_directory() {
        let engine = WorkflowEngine::new();
        // Load agents first so workflow references resolve.
        let agents_dir = format!("{}/../../agents/", env!("CARGO_MANIFEST_DIR"));
        engine.load_agents(&agents_dir).await.unwrap();

        let workflows_dir = format!("{}/../../workflows/", env!("CARGO_MANIFEST_DIR"));
        let result = engine.load_workflows(&workflows_dir).await;
        assert!(result.is_ok());
        let count = result.unwrap();
        assert!(count >= 1, "Should load at least 1 workflow");
    }

    #[tokio::test]
    async fn workflow_resolves_agent_refs() {
        let engine = WorkflowEngine::new();
        let agents_dir = format!("{}/../../agents/", env!("CARGO_MANIFEST_DIR"));
        engine.load_agents(&agents_dir).await.unwrap();

        let workflows_dir = format!("{}/../../workflows/", env!("CARGO_MANIFEST_DIR"));
        engine.load_workflows(&workflows_dir).await.unwrap();

        let workflows = engine.list_workflows().await;
        assert!(!workflows.is_empty());

        let default_wf = workflows.iter().find(|w| w.id == "default");
        assert!(default_wf.is_some(), "should have a 'default' workflow");

        let wf = default_wf.unwrap();
        assert_eq!(wf.on, "channel.*.inbound");
        assert!(wf.agents.contains_key("assistant"));
    }

    #[tokio::test]
    async fn get_agent_returns_v2_definition() {
        let engine = WorkflowEngine::new();
        let agents_dir = format!("{}/../../agents/", env!("CARGO_MANIFEST_DIR"));
        engine.load_agents(&agents_dir).await.unwrap();

        let agent = engine.get_agent("personal-assistant").await;
        assert!(agent.is_some());
        let def = agent.unwrap();
        assert_eq!(def.scope, "scope_root");
        assert!(!def.tools.is_empty());
    }

    #[tokio::test]
    async fn get_nonexistent_agent_returns_none() {
        let engine = WorkflowEngine::new();
        let agent = engine.get_agent("does-not-exist").await;
        assert!(agent.is_none());
    }

    #[tokio::test]
    async fn load_agents_bad_directory() {
        let engine = WorkflowEngine::new();
        let result = engine.load_agents("/nonexistent/path/").await;
        assert!(result.is_err());
    }

    #[test]
    fn personal_assistant_pipeline_validation() {
        let engine = WorkflowEngine::new();
        let agent = make_agent(
            vec![
                make_step("content-ingester", "trigger", "brain.content.stored"),
                make_step("graph-context-loader", "trigger", "agent.context.loaded"),
                make_step("llm-router", "agent.context.loaded", "llm.response"),
                make_step("event-emitter", "llm.response", "task.created"),
                make_step("response-formatter", "llm.response", "channel.*.outbound"),
            ],
            vec![
                "channel.telegram.inbound",
                "channel.cli.inbound",
                "task.completed.*",
            ],
        );
        let warnings = engine.validate_pipeline(&agent);
        assert!(
            warnings.is_empty(),
            "expected no warnings, got: {:?}",
            warnings
        );
    }
}
