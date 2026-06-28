use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use seidrum_agent_runtime::{
    replay_job_run, AgentProvider, AgentRuntime, DurableJobStore, ForceRunJobExecutor,
    JobDefinition, JobRunStatus, ProviderRequest, ProviderResponse, RedbTurnStore, RuntimeConfig,
    RuntimeError,
};
use serde_json::json;

#[derive(Clone)]
struct ScriptedProvider {
    responses: Arc<Mutex<VecDeque<Result<ProviderResponse, RuntimeError>>>>,
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}

impl ScriptedProvider {
    fn new(responses: impl IntoIterator<Item = Result<ProviderResponse, RuntimeError>>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into_iter().collect())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn requests(&self) -> Vec<ProviderRequest> {
        self.requests
            .lock()
            .expect("requests lock poisoned")
            .clone()
    }
}

#[async_trait]
impl AgentProvider for ScriptedProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, RuntimeError> {
        self.requests
            .lock()
            .expect("requests lock poisoned")
            .push(request);
        self.responses
            .lock()
            .expect("responses lock poisoned")
            .pop_front()
            .expect("scripted response should exist")
    }
}

fn job_definition(job_id: &str) -> JobDefinition {
    JobDefinition {
        job_id: job_id.to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("session-job-1".to_string()),
        prompt: "run the durable job".to_string(),
        schedule_spec: Some("once:manual".to_string()),
        enabled: true,
        metadata: json!({ "source": "test" }),
    }
}

#[tokio::test]
async fn durable_jobs_persist_definitions_across_reopen() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join("runtime.redb");

    let store = RedbTurnStore::open(&path).expect("store should open");
    store
        .put_job_definition(job_definition("job-1"))
        .await
        .expect("job definition should persist");
    drop(store);

    let reopened = RedbTurnStore::open(&path).expect("store should reopen");
    let loaded = reopened
        .job_definition("job-1")
        .await
        .expect("definition lookup should succeed")
        .expect("definition should exist after reopen");
    let listed = reopened
        .list_job_definitions()
        .await
        .expect("definitions should list");

    assert_eq!(loaded.job_id, "job-1");
    assert_eq!(loaded.agent_id, "agent-1");
    assert_eq!(loaded.session_id.as_deref(), Some("session-job-1"));
    assert_eq!(loaded.schedule_spec.as_deref(), Some("once:manual"));
    assert!(loaded.enabled);
    assert_eq!(loaded.metadata, json!({ "source": "test" }));
    assert_eq!(listed, vec![loaded]);
}

#[tokio::test]
async fn force_run_job_persists_succeeded_run_and_trace() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join("runtime.redb");
    let store = RedbTurnStore::open(&path).expect("store should open");
    store
        .put_job_definition(job_definition("job-1"))
        .await
        .expect("job definition should persist");
    let provider = ScriptedProvider::new([Ok(ProviderResponse::final_text("job finished"))]);
    let runtime = AgentRuntime::new(provider.clone(), store.clone(), RuntimeConfig::default());
    let executor = ForceRunJobExecutor::new(runtime, store.clone());

    let run = executor
        .force_run_job("job-1")
        .await
        .expect("job should force-run");

    assert_eq!(run.job_id, "job-1");
    assert_eq!(run.status, JobRunStatus::Succeeded);
    assert_eq!(run.session_id, "session-job-1");
    assert_eq!(run.final_text.as_deref(), Some("job finished"));
    assert_eq!(run.output_summary.as_deref(), Some("job finished"));
    assert_eq!(run.error, None);
    assert_eq!(run.trace_session_id, "session-job-1");
    assert!(run.started_sequence > 0);
    assert!(run.completed_sequence.unwrap() >= run.started_sequence);

    let persisted = store
        .job_run(&run.run_id)
        .await
        .expect("run lookup should succeed")
        .expect("run should persist");
    assert_eq!(persisted, run);
    let job_runs = store
        .list_job_runs("job-1")
        .await
        .expect("job runs should list");
    assert_eq!(job_runs, vec![run.clone()]);
    assert_eq!(provider.requests().len(), 1);

    let trace = seidrum_agent_runtime::replay_session_trace(&store, "session-job-1")
        .await
        .expect("trace should replay");
    assert_eq!(trace.records.len(), 2);
}

#[tokio::test]
async fn force_run_job_records_provider_failure_as_failed_run() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join("runtime.redb");
    let store = RedbTurnStore::open(&path).expect("store should open");
    store
        .put_job_definition(job_definition("job-1"))
        .await
        .expect("job definition should persist");
    let provider =
        ScriptedProvider::new([Err(RuntimeError::Provider("provider down".to_string()))]);
    let runtime = AgentRuntime::new(provider, store.clone(), RuntimeConfig::default());
    let executor = ForceRunJobExecutor::new(runtime, store.clone());

    let run = executor
        .force_run_job("job-1")
        .await
        .expect("provider failure should be recorded, not dropped");

    assert_eq!(run.status, JobRunStatus::Failed);
    assert_eq!(run.final_text, None);
    assert_eq!(run.error.as_deref(), Some("provider error: provider down"));
    assert_eq!(run.trace_session_id, "session-job-1");

    let persisted = store
        .job_run(&run.run_id)
        .await
        .expect("run lookup should succeed")
        .expect("failed run should persist");
    assert_eq!(persisted.status, JobRunStatus::Failed);
    assert_eq!(
        persisted.error.as_deref(),
        Some("provider error: provider down")
    );

    let trace = seidrum_agent_runtime::replay_session_trace(&store, "session-job-1")
        .await
        .expect("partial failure trace should replay");
    assert_eq!(trace.records.len(), 1);
}

#[tokio::test]
async fn job_run_replay_loads_trace_without_provider_or_tool() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join("runtime.redb");
    let store = RedbTurnStore::open(&path).expect("store should open");
    store
        .put_job_definition(job_definition("job-1"))
        .await
        .expect("job definition should persist");
    let provider = ScriptedProvider::new([Ok(ProviderResponse::final_text("job finished"))]);
    let runtime = AgentRuntime::new(provider.clone(), store.clone(), RuntimeConfig::default());
    let executor = ForceRunJobExecutor::new(runtime, store.clone());
    let run = executor
        .force_run_job("job-1")
        .await
        .expect("job should force-run");
    assert_eq!(provider.requests().len(), 1);
    drop(executor);
    drop(store);

    let replay_store = RedbTurnStore::open(&path).expect("store should reopen");
    let replay = replay_job_run(&replay_store, &run.run_id)
        .await
        .expect("job run should replay");

    assert_eq!(replay.run.run_id, run.run_id);
    assert_eq!(replay.run.status, JobRunStatus::Succeeded);
    assert_eq!(replay.trace.session_id, "session-job-1");
    assert_eq!(replay.trace.records.len(), 2);
    assert_eq!(
        provider.requests().len(),
        1,
        "replay must not call provider"
    );
}
