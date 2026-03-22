//! Scheduler service: confidence decay and health monitoring.
//!
//! Uses `tokio-cron-scheduler` for periodic jobs:
//! - **Confidence decay**: daily at 3:00 AM, applies the decay AQL from BRAIN_SCHEMA.md
//! - **Health monitoring**: every 30 seconds, pings registered plugins and publishes system.health

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{error, info, warn};

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use seidrum_common::events::{
    DecayCompleted, EventEnvelope, PluginDeregister, PluginHealthRequest, PluginHealthResponse,
    SystemHealth,
};

use crate::brain::client::ArangoClient;
use crate::registry::service::RegistryService;

/// The AQL query that decays confidence on unreinforced facts.
/// From BRAIN_SCHEMA.md — "Confidence Decay (scheduled by kernel)".
const DECAY_AQL: &str = r#"
LET decay_threshold = DATE_SUBTRACT(DATE_NOW(), 90, "day")
FOR fact IN facts
  FILTER fact.valid_to == null AND fact.superseded_by == null
     AND fact.last_reinforced < decay_threshold AND fact.confidence > 0.1
  LET days_since = DATE_DIFF(fact.last_reinforced, DATE_NOW(), "day")
  LET new_confidence = MAX(0.1, fact.confidence - (days_since * 0.005))
  UPDATE fact WITH { confidence: new_confidence } IN facts
  RETURN 1
"#;

/// Scheduler service that manages periodic kernel jobs.
pub struct SchedulerService {
    arango: ArangoClient,
    nats: async_nats::Client,
    registry: RegistryService,
    start_time: chrono::DateTime<Utc>,
}

impl SchedulerService {
    /// Create a new scheduler service.
    pub fn new(
        arango: ArangoClient,
        nats: async_nats::Client,
        registry: RegistryService,
    ) -> Self {
        Self {
            arango,
            nats,
            registry,
            start_time: Utc::now(),
        }
    }

    /// Start the scheduler. Returns a `JoinHandle` that runs until shutdown.
    pub async fn spawn(self) -> Result<tokio::task::JoinHandle<()>> {
        let scheduler = JobScheduler::new()
            .await
            .context("failed to create job scheduler")?;

        // -----------------------------------------------------------------
        // Job 1: Confidence Decay — daily at 03:00
        // -----------------------------------------------------------------
        let decay_arango = self.arango.clone();
        let decay_nats = self.nats.clone();

        let decay_job = Job::new_async("0 0 3 * * *", move |_uuid, _lock| {
            let arango = decay_arango.clone();
            let nats = decay_nats.clone();
            Box::pin(async move {
                info!("scheduler: starting confidence decay job");
                let start = std::time::Instant::now();

                match run_confidence_decay(&arango).await {
                    Ok(facts_decayed) => {
                        let duration_ms = start.elapsed().as_millis() as u64;
                        info!(
                            facts_decayed,
                            duration_ms, "scheduler: confidence decay completed"
                        );

                        let payload = DecayCompleted {
                            facts_decayed,
                            facts_archived: 0,
                            duration_ms,
                        };

                        if let Err(e) = publish_event(
                            &nats,
                            "system.maintenance.decay",
                            "kernel-scheduler",
                            &payload,
                        )
                        .await
                        {
                            error!(error = %e, "scheduler: failed to publish decay event");
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "scheduler: confidence decay failed");
                    }
                }
            })
        })
        .context("failed to create decay cron job")?;

        scheduler
            .add(decay_job)
            .await
            .context("failed to add decay job to scheduler")?;
        info!("scheduler: confidence decay job registered (daily at 03:00)");

        // -----------------------------------------------------------------
        // Job 2: Health Monitoring — every 30 seconds
        // -----------------------------------------------------------------
        let health_nats = self.nats.clone();
        let health_registry = self.registry.clone();
        let health_start_time = self.start_time;
        let failure_counts: Arc<Mutex<HashMap<String, u32>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let health_job = Job::new_async("0/30 * * * * *", move |_uuid, _lock| {
            let nats = health_nats.clone();
            let registry = health_registry.clone();
            let start_time = health_start_time;
            let failures = failure_counts.clone();
            Box::pin(async move {
                if let Err(e) =
                    run_health_check(&nats, &registry, start_time, &failures).await
                {
                    error!(error = %e, "scheduler: health check failed");
                }
            })
        })
        .context("failed to create health check cron job")?;

        scheduler
            .add(health_job)
            .await
            .context("failed to add health job to scheduler")?;
        info!("scheduler: health monitoring job registered (every 30s)");

        // -----------------------------------------------------------------
        // Start the scheduler
        // -----------------------------------------------------------------
        scheduler
            .start()
            .await
            .context("failed to start scheduler")?;
        info!("scheduler: started");

        let handle = tokio::spawn(async move {
            // Keep the scheduler alive. It runs its own internal tasks; we
            // just hold the handle so it is not dropped. When the task is
            // cancelled, `_scheduler` is dropped and shuts down gracefully.
            let _scheduler = scheduler;
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        });

        Ok(handle)
    }
}

/// Execute the confidence decay AQL query and return the number of facts updated.
async fn run_confidence_decay(arango: &ArangoClient) -> Result<u32> {
    let resp = arango
        .execute_aql(DECAY_AQL, &serde_json::json!({}))
        .await
        .context("confidence decay AQL failed")?;

    // The query returns one `1` per updated document. Count them.
    let count = resp
        .get("result")
        .and_then(|v| v.as_array())
        .map(|arr| arr.len() as u32)
        .unwrap_or(0);

    Ok(count)
}

/// Number of consecutive health check failures before auto-deregistering a plugin.
/// At 30s intervals, 3 failures = 90 seconds unresponsive.
const MAX_HEALTH_FAILURES: u32 = 3;

/// Ping every registered plugin via NATS request/reply and publish a SystemHealth event.
/// Tracks consecutive failures and auto-deregisters unresponsive plugins.
async fn run_health_check(
    nats: &async_nats::Client,
    registry: &RegistryService,
    start_time: chrono::DateTime<Utc>,
    failure_counts: &Mutex<HashMap<String, u32>>,
) -> Result<()> {
    let plugins = registry.list_plugins().await;
    let mut active_plugins: Vec<String> = Vec::new();
    let mut failed_plugins: Vec<String> = Vec::new();

    for plugin in &plugins {
        let health_subject = &plugin.health_subject;

        // Send a health ping with a 5-second timeout.
        let request_payload =
            serde_json::to_vec(&PluginHealthRequest {}).unwrap_or_default();

        let is_responsive = match tokio::time::timeout(
            Duration::from_secs(5),
            nats.request(health_subject.clone(), request_payload.into()),
        )
        .await
        {
            Ok(Ok(response)) => {
                match serde_json::from_slice::<PluginHealthResponse>(&response.payload) {
                    Ok(health) => {
                        if health.status != "healthy" {
                            warn!(
                                plugin_id = %plugin.id,
                                status = %health.status,
                                last_error = ?health.last_error,
                                "plugin reports non-healthy status"
                            );
                        }
                        // Any response = responsive
                        true
                    }
                    Err(e) => {
                        warn!(
                            plugin_id = %plugin.id,
                            error = %e,
                            "failed to parse health response from plugin"
                        );
                        false
                    }
                }
            }
            Ok(Err(e)) => {
                warn!(
                    plugin_id = %plugin.id,
                    error = %e,
                    "health ping failed for plugin"
                );
                false
            }
            Err(_) => {
                warn!(
                    plugin_id = %plugin.id,
                    "plugin health ping timed out (5s)"
                );
                false
            }
        };

        if is_responsive {
            active_plugins.push(plugin.id.clone());
        } else {
            failed_plugins.push(plugin.id.clone());
        }
    }

    // Update failure counts and auto-deregister dead plugins
    {
        let mut counts = failure_counts.lock().await;

        // Reset counters for responsive plugins
        for id in &active_plugins {
            counts.remove(id);
        }

        // Increment counters for failed plugins
        for id in &failed_plugins {
            let count = counts.entry(id.clone()).or_insert(0);
            *count += 1;

            if *count >= MAX_HEALTH_FAILURES {
                warn!(
                    plugin_id = %id,
                    consecutive_failures = *count,
                    "auto-deregistering unresponsive plugin"
                );

                // Publish plugin.deregister — the registry and tool registry
                // both subscribe to this and will clean up
                let dereg = PluginDeregister { id: id.clone() };
                if let Ok(bytes) = serde_json::to_vec(&dereg) {
                    if let Err(e) = nats.publish("plugin.deregister", bytes.into()).await {
                        error!(error = %e, plugin_id = %id, "failed to publish auto-deregister");
                    }
                }

                counts.remove(id);
            }
        }
    }

    let uptime_seconds = (Utc::now() - start_time).num_seconds().max(0) as u64;

    let system_health = SystemHealth {
        nats_connected: true,
        arangodb_connected: true,
        active_plugins,
        active_agents: 0,
        uptime_seconds,
    };

    publish_event(nats, "system.health", "kernel-scheduler", &system_health).await?;

    Ok(())
}

/// Helper to wrap a payload in an EventEnvelope and publish to NATS.
async fn publish_event<T: serde::Serialize>(
    nats: &async_nats::Client,
    subject: &str,
    source: &str,
    payload: &T,
) -> Result<()> {
    let envelope = EventEnvelope::new(subject, source, None, None, payload)
        .context("failed to create event envelope")?;
    let bytes = serde_json::to_vec(&envelope).context("failed to serialize envelope")?;
    nats.publish(subject.to_string(), bytes.into())
        .await
        .context("failed to publish event")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decay_aql_is_valid_string() {
        // Ensure the AQL constant is non-empty and contains key keywords.
        assert!(DECAY_AQL.contains("facts"));
        assert!(DECAY_AQL.contains("confidence"));
        assert!(DECAY_AQL.contains("decay_threshold"));
        assert!(DECAY_AQL.contains("UPDATE"));
    }

    #[test]
    fn decay_completed_roundtrip() {
        let event = DecayCompleted {
            facts_decayed: 42,
            facts_archived: 0,
            duration_ms: 150,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: DecayCompleted = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.facts_decayed, 42);
        assert_eq!(deserialized.duration_ms, 150);
    }

    #[test]
    fn system_health_roundtrip() {
        let event = SystemHealth {
            nats_connected: true,
            arangodb_connected: true,
            active_plugins: vec!["telegram".to_string(), "llm-router".to_string()],
            active_agents: 1,
            uptime_seconds: 3600,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: SystemHealth = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.active_plugins.len(), 2);
        assert!(deserialized.nats_connected);
        assert_eq!(deserialized.uptime_seconds, 3600);
    }

    #[test]
    fn plugin_health_request_roundtrip() {
        let req = PluginHealthRequest {};
        let json = serde_json::to_string(&req).unwrap();
        let _: PluginHealthRequest = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn plugin_health_response_roundtrip() {
        let resp = PluginHealthResponse {
            plugin_id: "telegram".to_string(),
            status: "healthy".to_string(),
            uptime_seconds: 600,
            events_processed: 42,
            last_error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let deserialized: PluginHealthResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.plugin_id, "telegram");
        assert_eq!(deserialized.status, "healthy");
    }
}
