use std::{
    env, fs,
    hint::black_box,
    io::{self, Write},
    time::{Duration, Instant},
};

use loom_core::{
    AgentSessionId, AgentSessionSnapshot, AgentSessionState, ApprovalPolicy, EventSequence,
    RunAttemptId, RunId, SessionLimits, Timestamp, UsageSnapshot, WorkspaceId,
};
use loom_model::ModelId;
use loom_persistence::{
    CURRENT_SCHEMA_VERSION, DurableFeedState, DurableRunMessage, DurableRunRuntimeConfig,
    DurableRunSummary, DurableStateWrite, FilePersistence,
};
use loom_protocol::{
    AgentRunSnapshot, AgentRunState, CURRENT_PROTOCOL_VERSION, ContextAssemblyOptions, ServerEvent,
    ServerEventEnvelope,
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
        .unwrap_or(session_count)
        .max(1);
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
    let mut run_runtime_configs = std::collections::BTreeMap::new();
    let mut run_messages = std::collections::BTreeMap::new();
    let mut run_ids = Vec::with_capacity(run_count);
    let system_instructions =
        "Follow repository guidance, make focused changes, and verify behavior. ".repeat(8);
    let repository_instructions =
        "Prefer the existing code patterns and keep generated files unchanged. ".repeat(6);
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
        run_ids.push(run_id);
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
        run_runtime_configs.insert(
            run_id,
            DurableRunRuntimeConfig {
                system_instructions: Some(system_instructions.clone()),
                repository_instructions: Some(repository_instructions.clone()),
                approval_policy: ApprovalPolicy::default(),
                limits: SessionLimits {
                    max_tool_calls: Some(100),
                    ..SessionLimits::default()
                },
                context_options: ContextAssemblyOptions {
                    context_window: Some(32_768),
                    max_input_tokens: Some(24_000),
                    reserved_output_tokens: Some(4_096),
                },
                checkpoint_id: None,
                input_cost_micros_per_1k: 1,
                output_cost_micros_per_1k: 3,
                context_inspection: None,
            },
        );
        let tool_output = (0..12)
            .map(|line| {
                format!(
                    "run {index}: test_module_{line} passed; fixtures and assertions verified\n"
                )
            })
            .collect::<String>();
        run_messages.insert(
            run_id,
            vec![
                DurableRunMessage {
                    role: loom_model::MessageRole::User,
                    content: format!(
                        "Inspect workspace item {index}, make a focused change, and run its tests."
                    ),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
                DurableRunMessage {
                    role: loom_model::MessageRole::Assistant,
                    content: format!(
                        "I updated the relevant files for item {index} and am checking the result."
                    ),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
                DurableRunMessage {
                    role: loom_model::MessageRole::Tool,
                    content: tool_output,
                    name: Some("run_tests".to_owned()),
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
                DurableRunMessage {
                    role: loom_model::MessageRole::Assistant,
                    content: format!(
                        "Validation completed for item {index}. {}",
                        "No failures were reported. ".repeat(8)
                    ),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
            ],
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
        run_runtime_configs: Some(&run_runtime_configs),
        run_context_checkpoints: None,
        run_plans: None,
        run_messages: Some(&run_messages),
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
    let mut transcript_page_samples = Vec::with_capacity(5);
    let mut content_range_samples = Vec::with_capacity(5);
    let target_run_id = run_ids.first().copied().ok_or("missing target run")?;
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
        let transcript_page_started = Instant::now();
        let transcript_page = fresh_handle.load_run_message_page(target_run_id, None, 20)?;
        transcript_page_samples.push(transcript_page_started.elapsed());
        let content_range_started = Instant::now();
        let newest_message = transcript_page.first().ok_or("missing transcript page")?;
        let message_content = fresh_handle.load_run_message_content_range(
            target_run_id,
            newest_message.ordinal,
            0,
            usize::try_from(newest_message.content_bytes)?.min(4096),
        )?;
        content_range_samples.push(content_range_started.elapsed());
        black_box((
            loaded_sessions,
            active_runs,
            feed_header,
            stream_events,
            full_feed,
            transcript_page,
            message_content,
        ));
    }
    startup_samples.sort_unstable();
    stream_samples.sort_unstable();
    full_feed_samples.sort_unstable();
    transcript_page_samples.sort_unstable();
    content_range_samples.sort_unstable();

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
    writeln!(
        output,
        "typed run runtime configs: {}",
        run_runtime_configs.len()
    )?;
    writeln!(
        output,
        "transcript messages: {}",
        run_messages.values().map(Vec::len).sum::<usize>()
    )?;
    writeln!(output, "durable reconnect events: {}", feed.events.len())?;
    writeln!(output, "database bytes: {database_bytes}")?;
    let connection = Connection::open(&path)?;
    let runtime_profile_count: i64 =
        connection.query_row("SELECT COUNT(*) FROM runtime_configurations", [], |row| {
            row.get(0)
        })?;
    writeln!(
        output,
        "deduplicated runtime profiles: {runtime_profile_count}"
    )?;
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
        "newest transcript page p50 (20-message page): {}",
        fmt(transcript_page_samples[2])
    )?;
    writeln!(
        output,
        "bounded transcript content read p50: {}",
        fmt(content_range_samples[2])
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
