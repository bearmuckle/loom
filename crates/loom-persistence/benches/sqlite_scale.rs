use std::{
    env, fs,
    hint::black_box,
    io::{self, Write},
    time::{Duration, Instant},
};

use loom_core::{
    AgentSessionId, AgentSessionSnapshot, AgentSessionState, EventSequence, RunAttemptId, RunId,
    Timestamp, UsageSnapshot, WorkspaceId,
};
use loom_model::ModelId;
use loom_persistence::{
    CURRENT_SCHEMA_VERSION, DurableFeedState, DurableRunSummary, DurableStateWrite, FilePersistence,
};
use loom_protocol::{
    AgentRunSnapshot, AgentRunState, CURRENT_PROTOCOL_VERSION, ServerEvent, ServerEventEnvelope,
};
use loom_session::SessionManager;
use rusqlite::Connection;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let session_count = env::var("LOOM_SCALE_SESSIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(10_000)
        .max(1);
    let run_count = env::var("LOOM_SCALE_RUNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(session_count);
    let path = env::temp_dir().join(format!("loom-sqlite-scale-{}.db", std::process::id()));
    let _ = fs::remove_file(&path);

    let workspace_id = WorkspaceId::new();
    let mut sessions = SessionManager::default();
    let mut session_ids = Vec::with_capacity(session_count);
    for index in 0..session_count {
        let (session, _) =
            sessions.create_in_workspace(workspace_id, format!("Session {index}"))?;
        session_ids.push(session.id);
    }

    let now = Timestamp::from_unix_millis(1_750_000_000_000);
    let mut run_summaries = std::collections::BTreeMap::new();
    for index in 0..run_count {
        let run_id = RunId::new();
        let session_id = session_ids[index % session_ids.len().max(1)];
        let active = index % 100 == 0;
        let state = if active {
            AgentRunState::Executing
        } else {
            AgentRunState::Completed
        };
        let snapshot = AgentRunSnapshot {
            id: run_id,
            attempt_id: RunAttemptId::new(),
            control_revision: 0,
            session_id,
            task: format!("Representative run {index}"),
            model: ModelId::new("scale-benchmark"),
            state,
            started_at: now,
            updated_at: now,
            completed_at: (!active).then_some(now),
            summary: None,
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

    let session_state = sessions.export_state();
    let feed_sequence = u64::try_from(session_count)?;
    let feed = DurableFeedState {
        next_sequence: EventSequence::new(feed_sequence),
        retention_limit: 4096,
        events: session_ids
            .iter()
            .enumerate()
            .map(
                |(index, session_id)| -> Result<_, Box<dyn std::error::Error>> {
                    let sequence = u64::try_from(index + 1)?;
                    let snapshot = AgentSessionSnapshot {
                        id: *session_id,
                        workspace_id,
                        name: format!("Session {index}"),
                        state: AgentSessionState::Idle,
                        created_at: now,
                        updated_at: now,
                    };
                    Ok(ServerEventEnvelope {
                        protocol_version: CURRENT_PROTOCOL_VERSION,
                        sequence: EventSequence::new(sequence),
                        session_id: *session_id,
                        event: ServerEvent::AgentSessionCreated { snapshot },
                    })
                },
            )
            .collect::<Result<Vec<_>, _>>()?,
    };
    let persistence = FilePersistence::open(&path)?;
    let write_started = Instant::now();
    persistence.save_state(DurableStateWrite {
        schema_version: CURRENT_SCHEMA_VERSION,
        sessions: &session_state,
        workspaces: None,
        settings: None,
        workspace_configs: None,
        providers: None,
        usage: None,
        idempotency: None,
        run_summaries: Some(&run_summaries),
        run_context_checkpoints: None,
        run_plans: None,
        run_messages: None,
        run_activities: None,
        filesystem_records: None,
        records: &[],
        feed: Some(&feed),
        sections: &[],
    })?;
    let write_elapsed = write_started.elapsed();
    let database_bytes = fs::metadata(&path)?.len();
    drop(persistence);

    let target_session = session_ids
        .first()
        .copied()
        .unwrap_or_else(AgentSessionId::new);
    let mut startup_samples = Vec::with_capacity(5);
    let mut stream_samples = Vec::with_capacity(5);
    let mut full_feed_samples = Vec::with_capacity(5);
    for _ in 0..5 {
        let fresh_handle = FilePersistence::open(&path)?;
        let started = Instant::now();
        let loaded_sessions = fresh_handle.load_sessions()?.ok_or("missing sessions")?;
        let active_runs = fresh_handle.load_active_run_summaries()?;
        let feed_header = fresh_handle.load_feed_header()?.ok_or("missing feed")?;
        startup_samples.push(started.elapsed());
        let stream_started = Instant::now();
        let stream_events = fresh_handle.load_feed_events_since(Some(target_session), None)?;
        stream_samples.push(stream_started.elapsed());
        let full_feed_started = Instant::now();
        let full_feed = fresh_handle.load_feed_state()?.ok_or("missing feed")?;
        full_feed_samples.push(full_feed_started.elapsed());
        black_box((
            loaded_sessions,
            active_runs,
            feed_header,
            stream_events,
            full_feed,
        ));
    }
    startup_samples.sort_unstable();
    stream_samples.sort_unstable();
    full_feed_samples.sort_unstable();

    let lookup_handle = FilePersistence::open(&path)?;
    let mut lookup_samples = Vec::with_capacity(100);
    for _ in 0..100 {
        let started = Instant::now();
        black_box(lookup_handle.load_run_summaries_for_session(target_session)?);
        lookup_samples.push(started.elapsed());
    }
    lookup_samples.sort_unstable();

    let mut output = io::stdout().lock();
    writeln!(output, "storage scale benchmark")?;
    writeln!(output, "sessions: {session_count}; runs: {run_count}")?;
    writeln!(output, "durable reconnect events: {}", feed.events.len())?;
    writeln!(output, "database bytes: {database_bytes}")?;
    let connection = Connection::open(&path)?;
    let mut statement = connection.prepare(
        "SELECT name, SUM(pgsize) FROM dbstat GROUP BY name ORDER BY SUM(pgsize) DESC LIMIT 12",
    )?;
    let storage_objects = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    writeln!(output, "largest SQLite storage objects:")?;
    for (name, bytes) in storage_objects {
        writeln!(output, "  {name}: {bytes} bytes")?;
    }
    writeln!(output, "one-transaction population: {}", fmt(write_elapsed))?;
    writeln!(
        output,
        "fresh-handle session+active-run+feed-header load p50: {}",
        fmt(startup_samples[2])
    )?;
    writeln!(
        output,
        "fresh-handle one-session feed load p50: {}",
        fmt(stream_samples[2])
    )?;
    writeln!(
        output,
        "full retained feed decode p50 (diagnostic): {}",
        fmt(full_feed_samples[2])
    )?;
    writeln!(
        output,
        "per-session indexed run lookup p50 (100 queries): {}",
        fmt(lookup_samples[50])
    )?;

    drop(lookup_handle);
    fs::remove_file(path)?;
    Ok(())
}

fn fmt(duration: Duration) -> String {
    format!("{:.3} ms", duration.as_secs_f64() * 1000.0)
}
