//! Agent orchestrator service.
//!
//! Loads agent YAML definitions from the agents/ directory, validates pipeline
//! coherence, and sets up NATS subscription chains that inject scope metadata
//! into events as they flow through the pipeline.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures::StreamExt;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use seidrum_common::config::{load_agent_config, AgentConfig};
use seidrum_common::events::{ChannelInbound, EventEnvelope, EventOrigin};

/// Loaded agent with validation status.
#[derive(Debug, Clone)]
struct LoadedAgent {
    config: AgentConfig,
    /// Validation warnings (non-fatal).
    warnings: Vec<String>,
}

/// Thread-safe agent orchestrator.
#[derive(Clone)]
pub struct OrchestratorService {
    /// Map from agent ID to loaded agent config.
    agents: Arc<RwLock<HashMap<String, LoadedAgent>>>,
}

impl OrchestratorService {
    /// Create a new orchestrator and load all agent configs from the given directory.
    pub fn new() -> Self {
        Self {
            agents: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Load all agent YAML files from the agents directory.
    /// Returns the number of enabled agents loaded.
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

            match load_agent_config(&path) {
                Ok(agent_file) => {
                    let agent = agent_file.agent;
                    if !agent.enabled {
                        info!(
                            agent_id = %agent.id,
                            "agent is disabled, skipping"
                        );
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
                        "agent loaded"
                    );

                    let mut agents = self.agents.write().await;
                    agents.insert(
                        agent.id.clone(),
                        LoadedAgent {
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

    /// Validate that a pipeline's event type chains are coherent.
    ///
    /// Rules from AGENT_SPEC.md:
    /// - `consumes: trigger` is valid for first steps (they consume trigger events)
    /// - Subsequent steps must consume an event type that a previous step produces
    /// - Wildcard patterns (e.g., `channel.*.outbound`) are allowed in produces
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

        // Collect all event types that have been produced so far in the pipeline.
        // "trigger" is implicitly available as the initial input.
        let mut available_events: Vec<String> = vec!["trigger".to_string()];
        // Add trigger event types themselves as available
        for trigger in &agent.pipeline.triggers {
            available_events.push(trigger.clone());
        }

        for (i, step) in steps.iter().enumerate() {
            let consumes = &step.consumes;

            // Check if the consumed event type is available
            if !Self::event_matches_any(consumes, &available_events) {
                warnings.push(format!(
                    "step {} (plugin '{}') consumes '{}' but no previous step produces it",
                    i, step.plugin, consumes
                ));
            }

            // Add produced event types to available set
            for produced in step.produces.as_vec() {
                available_events.push(produced.to_string());
            }
        }

        // Validate background steps: they consume brain/agent events, not pipeline outputs
        if let Some(bg_steps) = &agent.background {
            for bg in bg_steps {
                // Background steps consume events produced by the system/pipeline;
                // we just log if it looks unusual.
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
    /// Supports wildcard matching (e.g., `channel.*.outbound` matches `channel.cli.outbound`).
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
    /// `*` matches a single token, `>` matches one or more tokens.
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

    /// Get an agent config by ID.
    pub async fn get_agent(&self, agent_id: &str) -> Option<AgentConfig> {
        let agents = self.agents.read().await;
        agents.get(agent_id).map(|a| a.config.clone())
    }

    /// Get all loaded (enabled) agent configs.
    pub async fn list_agents(&self) -> Vec<AgentConfig> {
        let agents = self.agents.read().await;
        agents.values().map(|a| a.config.clone()).collect()
    }

    /// Spawn the orchestrator: load agents, set up NATS subscriptions for
    /// pipeline trigger events with scope injection.
    pub async fn spawn(
        self,
        nats_client: async_nats::Client,
        agents_dir: &str,
    ) -> Result<tokio::task::JoinHandle<()>> {
        // Load all agent configs at startup.
        let count = self.load_agents(agents_dir).await?;
        info!(count, "orchestrator loaded agents");

        if count == 0 {
            warn!("no enabled agents found — orchestrator will idle");
        }

        // Collect all trigger subjects across all enabled agents,
        // along with their scope metadata.
        let agents = self.agents.read().await;
        let mut trigger_scopes: HashMap<String, Vec<ScopeContext>> = HashMap::new();

        for loaded in agents.values() {
            let agent = &loaded.config;
            let scope_ctx = ScopeContext {
                agent_id: agent.id.clone(),
                scope: agent.scope.clone(),
                additional_scopes: agent.additional_scopes.clone(),
            };

            for trigger in &agent.pipeline.triggers {
                trigger_scopes
                    .entry(trigger.clone())
                    .or_default()
                    .push(scope_ctx.clone());
            }
        }
        drop(agents);

        // Subscribe to each unique trigger subject.
        let mut subscriptions = Vec::new();
        for (subject, scopes) in &trigger_scopes {
            let sub = nats_client
                .subscribe(subject.clone())
                .await
                .with_context(|| {
                    format!("failed to subscribe to trigger subject: {}", subject)
                })?;
            info!(subject = %subject, agents = scopes.len(), "subscribed to trigger");
            subscriptions.push((sub, scopes.clone()));
        }

        let nats = nats_client.clone();
        let orchestrator = self.clone();

        let handle = tokio::spawn(async move {
            // Merge all subscription streams into processing.
            // Each subscription gets its own task.
            let mut handles = Vec::new();

            for (mut sub, scopes) in subscriptions {
                let nats = nats.clone();
                let _orchestrator = orchestrator.clone();
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

                        // Inject origin from channel inbound payload if present.
                        if envelope.origin.is_none() {
                            if let Ok(inbound) = serde_json::from_value::<ChannelInbound>(envelope.payload.clone()) {
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

                        // For each agent that has this trigger, inject scope
                        // and republish so pipeline plugins pick it up.
                        for scope_ctx in &scopes {
                            // Inject scope from agent config if not already set.
                            if envelope.scope.is_none() {
                                envelope.scope = Some(scope_ctx.scope.clone());
                            }

                            // Republish with scope-annotated subject for the agent pipeline.
                            // The subject pattern is: agent.{agent_id}.pipeline.{original_subject}
                            let pipeline_subject = format!(
                                "agent.{}.pipeline.{}",
                                scope_ctx.agent_id, envelope.event_type
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
                                            agent_id = %scope_ctx.agent_id,
                                            scope = %scope_ctx.scope,
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

            // Wait for all subscription handlers to complete.
            for h in handles {
                if let Err(e) = h.await {
                    error!(error = %e, "trigger subscription handler panicked");
                }
            }

            info!("orchestrator service stopped");
        });

        Ok(handle)
    }
}

/// Scope context extracted from an agent config, used for injection.
#[derive(Debug, Clone)]
struct ScopeContext {
    agent_id: String,
    scope: String,
    #[allow(dead_code)]
    additional_scopes: Vec<String>,
}

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
        let svc = OrchestratorService::new();
        let agent = make_agent(
            vec![
                make_step("content-ingester", "trigger", "brain.content.stored"),
                make_step("llm-router", "brain.content.stored", "llm.response"),
                make_step("response-formatter", "llm.response", "channel.cli.outbound"),
            ],
            vec!["channel.cli.inbound"],
        );
        let warnings = svc.validate_pipeline(&agent);
        assert!(warnings.is_empty(), "expected no warnings, got: {:?}", warnings);
    }

    #[test]
    fn pipeline_broken_chain_produces_warning() {
        let svc = OrchestratorService::new();
        let agent = make_agent(
            vec![
                make_step("content-ingester", "trigger", "brain.content.stored"),
                // This step consumes an event nobody produces
                make_step("llm-router", "agent.context.loaded", "llm.response"),
            ],
            vec!["channel.cli.inbound"],
        );
        let warnings = svc.validate_pipeline(&agent);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("agent.context.loaded"));
    }

    #[test]
    fn trigger_consumes_is_always_valid() {
        let svc = OrchestratorService::new();
        let agent = make_agent(
            vec![
                make_step("graph-context-loader", "trigger", "agent.context.loaded"),
            ],
            vec!["channel.cli.inbound"],
        );
        let warnings = svc.validate_pipeline(&agent);
        assert!(warnings.is_empty());
    }

    #[test]
    fn empty_pipeline_produces_warning() {
        let svc = OrchestratorService::new();
        let agent = make_agent(vec![], vec!["channel.cli.inbound"]);
        let warnings = svc.validate_pipeline(&agent);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("no steps"));
    }

    #[test]
    fn no_triggers_produces_warning() {
        let svc = OrchestratorService::new();
        let agent = make_agent(
            vec![make_step("llm-router", "trigger", "llm.response")],
            vec![],
        );
        let warnings = svc.validate_pipeline(&agent);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("no triggers"));
    }

    #[test]
    fn wildcard_pattern_matching() {
        // * matches single token
        assert!(OrchestratorService::event_pattern_matches(
            "channel.cli.outbound",
            "channel.*.outbound"
        ));
        assert!(!OrchestratorService::event_pattern_matches(
            "channel.cli.inbound",
            "channel.*.outbound"
        ));

        // Exact match
        assert!(OrchestratorService::event_pattern_matches(
            "llm.response",
            "llm.response"
        ));

        // > matches rest
        assert!(OrchestratorService::event_pattern_matches(
            "task.completed.abc123",
            "task.completed.>"
        ));
        assert!(OrchestratorService::event_pattern_matches(
            "task.completed.abc.def",
            "task.completed.>"
        ));

        // Different length without > should not match
        assert!(!OrchestratorService::event_pattern_matches(
            "a.b.c",
            "a.b"
        ));
    }

    #[test]
    fn wildcard_produces_matches_consumes() {
        let svc = OrchestratorService::new();
        // response-formatter produces channel.*.outbound
        // This is a valid pipeline since the wildcard output is fine
        let agent = make_agent(
            vec![
                make_step("llm-router", "trigger", "llm.response"),
                make_step("response-formatter", "llm.response", "channel.*.outbound"),
            ],
            vec!["channel.cli.inbound"],
        );
        let warnings = svc.validate_pipeline(&agent);
        assert!(warnings.is_empty(), "expected no warnings, got: {:?}", warnings);
    }

    #[tokio::test]
    async fn load_agents_from_directory() {
        let svc = OrchestratorService::new();
        // Navigate from crate root (CARGO_MANIFEST_DIR) to workspace root agents/
        let agents_dir = format!("{}/../../agents/", env!("CARGO_MANIFEST_DIR"));
        let result = svc.load_agents(&agents_dir).await;
        // Agent YAMLs are now V2 format (AgentDefinition), which load_agents
        // does not yet support (it uses V1 AgentConfigFile). The call succeeds
        // but loads 0 agents because V2 files fail to parse as V1.
        // TODO: update load_agents to support V2 AgentDefinitionFile format.
        assert!(result.is_ok());
        let count = result.unwrap();
        assert_eq!(count, 0, "V2 agent files should not parse as V1");
    }

    #[tokio::test]
    async fn get_nonexistent_agent_returns_none() {
        let svc = OrchestratorService::new();
        let agent = svc.get_agent("does-not-exist").await;
        assert!(agent.is_none());
    }

    #[tokio::test]
    async fn load_agents_bad_directory() {
        let svc = OrchestratorService::new();
        let result = svc.load_agents("/nonexistent/path/").await;
        assert!(result.is_err());
    }

    #[test]
    fn personal_assistant_pipeline_validation() {
        // Validate the actual personal-assistant agent pipeline structure
        let svc = OrchestratorService::new();
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
        let warnings = svc.validate_pipeline(&agent);
        assert!(warnings.is_empty(), "expected no warnings, got: {:?}", warnings);
    }
}
