//! Compare a one-message/one-activity worker checkpoint delta with rewriting
//! the complete transcript and activity history on every checkpoint.
//!
//! Fixture construction and initial database population are deliberately kept
//! outside the timed loops. Set LOOM_DELTA_ROWS and LOOM_DELTA_ITERATIONS to
//! adjust the fixture size and sample count.

use std::{env, fs, hint::black_box, path::PathBuf, time::Instant};

use loom_core::{
    ActivityId, AgentSessionSnapshot, EventSequence, RunAttemptId, RunId, Timestamp, UsageSnapshot,
    WorkspaceId,
};
use loom_model::{MessageRole, ModelId};
use loom_persistence::{
    DurableFeedState, DurableRunActivities, DurableRunCheckpointWrite, DurableRunMessage,
    DurableRunMessageDelta, DurableRunRuntimeConfig, DurableRunSummary, DurableStateWrite,
    FilePersistence,
};
use loom_protocol::{
    AgentActivityData, AgentActivityKind, AgentActivityRecord, AgentActivityStatus, AgentPlan,
    AgentRunSnapshot, AgentRunState, ContextAssemblyOptions,
};
use loom_session::SessionManager;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rows = env::var("LOOM_DELTA_ROWS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10_000usize)
        .max(1);
    let iterations = env::var("LOOM_DELTA_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(25usize)
        .max(1);
    let stem = env::temp_dir().join(format!("loom-checkpoint-delta-{}", std::process::id()));
    let delta_path = with_suffix(&stem, "delta");
    let full_path = with_suffix(&stem, "full");
    remove_database(&delta_path);
    remove_database(&full_path);

    let (session, run_id, summary, runtime_config, plan, messages, activities) = fixture(rows);
    let feed = DurableFeedState {
        next_sequence: EventSequence::new(1),
        retention_limit: 250,
        events: Vec::new(),
        workspace_events: Vec::new(),
    };
    seed(
        &delta_path,
        &session,
        run_id,
        &summary,
        &messages,
        &activities,
    )?;
    seed(
        &full_path,
        &session,
        run_id,
        &summary,
        &messages,
        &activities,
    )?;

    let delta_store = FilePersistence::open(&delta_path)?;
    let full_store = FilePersistence::open(&full_path)?;
    let mut delta_messages = messages.clone();
    let mut full_messages = messages.clone();
    let mut delta_activities = activities.clone();
    let mut full_activities = activities.clone();
    let mut delta_samples = Vec::with_capacity(iterations);
    let mut full_samples = Vec::with_capacity(iterations);

    for sample in 0..iterations {
        let appended = message(rows + sample, "delta checkpoint tail");
        delta_messages.push(appended.clone());
        full_messages.push(appended.clone());
        let updated_activity = activity(run_id, delta_activities[rows - 1].id, rows + sample);
        delta_activities[rows - 1] = updated_activity.clone();
        full_activities[rows - 1] = updated_activity.clone();
        let delta = DurableRunMessageDelta {
            start_ordinal: (rows + sample) as u64,
            reset: false,
            messages: vec![appended],
        };

        let started = Instant::now();
        delta_store.save_run_checkpoint(DurableRunCheckpointWrite {
            session: &session,
            session_next_sequence: EventSequence::new(1),
            prune_feed: false,
            summary: &summary,
            runtime_config: &runtime_config,
            context_checkpoint: None,
            plan: &plan,
            messages: &[],
            message_delta: Some(&delta),
            activities: &[],
            activity_deltas: Some(std::slice::from_ref(&updated_activity)),
            filesystem: None,
            feed: &feed,
        })?;
        delta_samples.push(started.elapsed());

        let started = Instant::now();
        full_store.save_run_checkpoint(DurableRunCheckpointWrite {
            session: &session,
            session_next_sequence: EventSequence::new(1),
            prune_feed: false,
            summary: &summary,
            runtime_config: &runtime_config,
            context_checkpoint: None,
            plan: &plan,
            messages: &full_messages,
            message_delta: None,
            activities: &full_activities,
            activity_deltas: None,
            filesystem: None,
            feed: &feed,
        })?;
        full_samples.push(started.elapsed());
        black_box((&delta_messages, &delta_activities));
    }

    let delta_bytes = database_and_wal_bytes(&delta_path);
    let full_bytes = database_and_wal_bytes(&full_path);
    println!("SQLite run checkpoint delta benchmark");
    println!("initial transcript rows: {rows}; initial activity rows: {rows}");
    println!("timed checkpoints: {iterations}");
    println!("delta checkpoint rows supplied per iteration: 1 transcript + 1 activity");
    println!(
        "full baseline rows supplied per iteration: {} transcript + {} activities (growing transcript includes tail)",
        rows + iterations.div_ceil(2),
        rows
    );
    println!(
        "delta checkpoint p50/p95: {}/{}",
        percentile(&mut delta_samples, 50),
        percentile(&mut delta_samples, 95)
    );
    println!(
        "full replacement p50/p95: {}/{}",
        percentile(&mut full_samples, 50),
        percentile(&mut full_samples, 95)
    );
    println!("final database + WAL bytes: delta={delta_bytes}, full={full_bytes}");

    drop((delta_store, full_store));
    remove_database(&delta_path);
    remove_database(&full_path);
    Ok(())
}

fn fixture(
    rows: usize,
) -> (
    AgentSessionSnapshot,
    RunId,
    DurableRunSummary,
    DurableRunRuntimeConfig,
    AgentPlan,
    Vec<DurableRunMessage>,
    Vec<AgentActivityRecord>,
) {
    let mut sessions = SessionManager::default();
    let (session, _) = sessions
        .create_in_workspace(WorkspaceId::new(), "checkpoint delta benchmark")
        .expect("session fixture");
    let run_id = RunId::new();
    let now = Timestamp::from_unix_millis(1_750_000_000_000);
    let summary = DurableRunSummary {
        snapshot: AgentRunSnapshot {
            id: run_id,
            attempt_id: RunAttemptId::new(),
            control_revision: 0,
            session_id: session.id,
            task: "large durable transcript benchmark".to_owned(),
            model: ModelId::new("checkpoint-delta-benchmark"),
            state: AgentRunState::Executing,
            started_at: now,
            updated_at: now,
            completed_at: None,
            summary: None,
            evidence: Vec::new(),
        },
        usage: UsageSnapshot::default(),
        attempts: None,
        execution_state: None,
        interactions: None,
    };
    let runtime_config = DurableRunRuntimeConfig {
        system_instructions: None,
        repository_instructions: None,
        approval_policy: Default::default(),
        limits: Default::default(),
        context_options: ContextAssemblyOptions::default(),
        checkpoint_id: None,
        input_cost_micros_per_1k: 0,
        output_cost_micros_per_1k: 0,
        context_inspection: None,
        project_delegation_enabled: false,
        project_messaging_enabled: false,
        project_inspection_enabled: false,
        project_child_control_enabled: false,
        project_worktree_enabled: false,
        project_review_enabled: false,
        project_integration_enabled: false,
        project_branch_messaging_enabled: false,
    };
    let messages = (0..rows)
        .map(|index| message(index, "representative persisted transcript row ".repeat(4)))
        .collect();
    let activities = (0..rows)
        .map(|index| activity(run_id, ActivityId::new(), index))
        .collect();
    (
        session,
        run_id,
        summary,
        runtime_config,
        AgentPlan { steps: Vec::new() },
        messages,
        activities,
    )
}

fn seed(
    path: &PathBuf,
    session: &AgentSessionSnapshot,
    run_id: RunId,
    summary: &DurableRunSummary,
    messages: &[DurableRunMessage],
    activities: &[AgentActivityRecord],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut sessions = SessionManager::default();
    sessions.create_in_workspace_with_id(session.workspace_id, session.id, session.name.clone())?;
    let summaries = std::collections::BTreeMap::from([(run_id, summary.clone())]);
    let run_messages = std::collections::BTreeMap::from([(run_id, messages.to_vec())]);
    let run_activities: DurableRunActivities =
        std::collections::BTreeMap::from([(run_id, activities.to_vec())]);
    FilePersistence::open(path)?.save_state(DurableStateWrite {
        sessions: &sessions.export_state(),
        workspaces: None,
        settings: None,
        workspace_configs: None,
        providers: None,
        usage: None,
        idempotency: None,
        run_summaries: Some(&summaries),
        run_runtime_configs: None,
        run_context_checkpoints: None,
        run_plans: None,
        run_messages: Some(&run_messages),
        run_activities: Some(&run_activities),
        filesystem_records: None,
        feed: None,
    })?;
    Ok(())
}

fn message(index: usize, content: impl Into<String>) -> DurableRunMessage {
    DurableRunMessage {
        role: MessageRole::Assistant,
        content: format!("{index}: {}", content.into()),
        name: None,
        tool_call_id: None,
        tool_calls: Vec::new(),
    }
}

fn activity(run_id: RunId, id: ActivityId, index: usize) -> AgentActivityRecord {
    let now = Timestamp::from_unix_millis(1_750_000_000_000 + index as u64);
    AgentActivityRecord {
        id,
        run_id,
        parent_id: None,
        step_id: None,
        kind: AgentActivityKind::ModelTurn,
        status: AgentActivityStatus::Completed,
        started_at: now,
        completed_at: Some(now),
        elapsed_ms: Some(100),
        data: AgentActivityData::ModelTurn {
            model: ModelId::new("checkpoint-delta-benchmark"),
        },
    }
}

fn percentile(samples: &mut [std::time::Duration], percentile: usize) -> String {
    samples.sort_unstable();
    let index = (samples.len() - 1) * percentile / 100;
    format!("{:?}", samples[index])
}

fn with_suffix(stem: &std::path::Path, suffix: &str) -> PathBuf {
    stem.with_extension(format!("{suffix}.db"))
}

fn remove_database(path: &std::path::Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(format!("{}-wal", path.display()));
    let _ = fs::remove_file(format!("{}-shm", path.display()));
}

fn database_and_wal_bytes(path: &std::path::Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
        + fs::metadata(format!("{}-wal", path.display())).map_or(0, |metadata| metadata.len())
}
