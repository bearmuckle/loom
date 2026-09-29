use std::{collections::BTreeMap, env, fs, time::Instant};

use loom_core::{
    Capability, CapabilitySet, CheckpointId, RunAttemptId, RunId, Timestamp, UsageSnapshot,
    WorkspaceId,
};
use loom_model::ModelId;
use loom_persistence::{
    DurableFilesystemRecord, DurableRunSummary, DurableStateWrite, FilePersistence,
};
use loom_protocol::{
    AgentRunSnapshot, AgentRunState, CURRENT_PROTOCOL_VERSION, Checkpoint, CheckpointFile,
    ClientRequest, ControlRequest, ControlResponse, RequestEnvelope, ServerResponse,
    SessionResponse, WorkspaceControl, WorkspaceRequest,
};
use loom_server::InProcessBackend;
use loom_session::{SessionManager, WorkspaceManager};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let session_count = env::var("LOOM_RESTORE_SESSIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(10_000)
        .max(1);
    let run_count = env::var("LOOM_RESTORE_RUNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(session_count)
        .max(1);
    let filesystem_stride = env::var("LOOM_RESTORE_FILESYSTEM_STRIDE")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(100)
        .max(1);
    let iterations = env::var("LOOM_RESTORE_ITERATIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(5)
        .max(1);
    let path = env::temp_dir().join(format!(
        "loom-server-restore-{}-{}.db",
        std::process::id(),
        WorkspaceId::new()
    ));

    let mut workspaces = WorkspaceManager::default();
    let workspace = workspaces.create("Restore benchmark")?;
    let mut sessions = SessionManager::default();
    let mut session_ids = Vec::with_capacity(session_count);
    for index in 0..session_count {
        let (session, _) =
            sessions.create_in_workspace(workspace.id, format!("Session {index}"))?;
        session_ids.push(session.id);
    }

    let now = Timestamp::now();
    let mut run_summaries = BTreeMap::new();
    for index in 0..run_count {
        let session_id = session_ids[index % session_ids.len()];
        let run_id = RunId::new();
        let snapshot = AgentRunSnapshot {
            id: run_id,
            attempt_id: RunAttemptId::new(),
            control_revision: 0,
            session_id,
            task: format!("Archived benchmark run {index}"),
            model: ModelId::new("restore-benchmark"),
            state: AgentRunState::Completed,
            started_at: now,
            updated_at: now,
            completed_at: Some(now),
            summary: Some("terminal history remains lazy during restore".to_owned()),
            evidence: Vec::new(),
        };
        run_summaries.insert(
            run_id,
            DurableRunSummary {
                snapshot,
                usage: UsageSnapshot::default(),
                attempts: None,
                execution_state: None,
                interactions: None,
            },
        );
    }

    let mut filesystem_records = Vec::new();
    for (index, session_id) in session_ids.iter().copied().enumerate() {
        if index % filesystem_stride != 0 {
            continue;
        }
        let checkpoint_id = CheckpointId::new();
        let checkpoint = Checkpoint {
            id: checkpoint_id,
            session_id,
            label: "restore benchmark".to_owned(),
            created_at: now,
            files: BTreeMap::from([(
                "src/main.rs".to_owned(),
                CheckpointFile {
                    existed: true,
                    content: "fn main() {}\n".to_owned(),
                    revision: "before".to_owned(),
                    expected_revision: "after".to_owned(),
                },
            )]),
        };
        filesystem_records.push(DurableFilesystemRecord {
            session_id,
            root: format!("/benchmark/session-{index}"),
            control: WorkspaceControl::User,
            checkpoints: vec![checkpoint],
            edits: Vec::new(),
            changes: Vec::new(),
            repositories: BTreeMap::new(),
            directories: Vec::new(),
            payload: serde_json::json!({
                "filesystem": {
                    "session_id": session_id,
                    "root": format!("/benchmark/session-{index}"),
                    "control": "user",
                    "checkpoints": [],
                    "edits": [],
                    "next_sequence": 0,
                    "changes": []
                }
            }),
            delta: None,
        });
    }

    let session_state = sessions.export_state();
    let workspace_state = workspaces.export_state();
    let persistence = FilePersistence::open(&path)?;
    persistence.save_state(DurableStateWrite {
        sessions: &session_state,
        workspaces: Some(&workspace_state),
        settings: None,
        workspace_configs: None,
        providers: None,
        usage: None,
        idempotency: None,
        run_summaries: Some(&run_summaries),
        run_runtime_configs: None,
        run_context_checkpoints: None,
        run_plans: None,
        run_messages: None,
        run_activities: None,
        filesystem_records: Some(&filesystem_records),
        feed: None,
    })?;
    drop(persistence);
    let bytes = [path.clone(), path.with_extension("db-wal")]
        .into_iter()
        .filter_map(|candidate| fs::metadata(candidate).ok())
        .map(|metadata| metadata.len())
        .sum::<u64>();

    let mut samples = Vec::with_capacity(iterations);
    for iteration in 0..iterations {
        let started = Instant::now();
        let backend = InProcessBackend::open_persistent(&path)?;
        let elapsed = started.elapsed();
        let connection = backend.connect();
        let negotiation = connection.request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::Negotiate {
                client_version: CURRENT_PROTOCOL_VERSION,
                capabilities: CapabilitySet::new([Capability::ReadAgentSession]),
            },
        )));
        if !matches!(
            negotiation.result,
            Ok(ServerResponse::Control(ControlResponse::Negotiated(_)))
        ) {
            return Err("restore benchmark client negotiation failed".into());
        }
        let listed = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::ListWorkspaceSessions {
                workspace_id: workspace.id,
                include_archived: true,
            },
        )));
        match listed.result? {
            ServerResponse::Session(SessionResponse::AgentSessions { sessions })
                if sessions.len() == session_count => {}
            other => return Err(format!("restore validation returned {other:?}").into()),
        }
        drop(backend);
        samples.push(elapsed);
        eprintln!(
            "restore sample {}: {:.3} ms",
            iteration + 1,
            elapsed.as_secs_f64() * 1000.0
        );
    }
    samples.sort_unstable();
    let p50 = samples[samples.len() / 2];
    println!(
        "fixture: sessions={session_count}, terminal_runs={run_count}, filesystems={}, checkpoints={}, database_bytes={bytes}",
        filesystem_records.len(),
        filesystem_records.len()
    );
    println!(
        "full InProcessBackend::open_persistent warm same-process p50: {:.3} ms ({} iterations); excludes UI construction and true cold disk cache",
        p50.as_secs_f64() * 1000.0,
        iterations
    );

    drop(run_summaries);
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(path.with_extension("db.lock"));
    let _ = fs::remove_file(path.with_extension("db-wal"));
    let _ = fs::remove_file(path.with_extension("db-shm"));
    Ok(())
}
