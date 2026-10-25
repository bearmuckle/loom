//! Tests for the `loom-cli` contract, driven through the in-process backend.
//!
//! Every test talks to a real `InProcessBackend` over the same `Client` seam the
//! binary uses, so the pinned output, exit codes, and cursor behaviour are the
//! behaviour of the shipped code rather than of a test double. The only test
//! doubles rewrite a protocol answer that would otherwise need a pruned journal
//! or a second worker.

use std::{path::PathBuf, sync::Arc, time::Duration};

use loom_core::{
    AgentSessionId, AgentSessionSnapshot, AgentSessionState, ApprovalPolicy, Author, Capability,
    CapabilitySet, CollaborationEntryKind, EventSequence, ProtocolVersion, RequestId, RunId,
    ToolCallId, WorkspaceId,
};
use loom_model::ModelId;
use loom_protocol::{
    ActivityDetail, AgentEvent, AgentRunSnapshot, AgentRunState, AgentSessionSnapshotProjection,
    CURRENT_PROTOCOL_VERSION, ClientRequest, CollaborationRequest, ControlRequest, ControlResponse,
    EventsRequest, EventsResponse, FilesystemRequest, FilesystemResponse, RepositoryRequest,
    RepositoryResponse, RequestEnvelope, ResponseEnvelope, RunRequest, RunResponse, ServerEvent,
    ServerEventEnvelope, ServerResponse, SessionDirectory, SessionRepository, SessionRequest,
    SessionResponse, WorkspaceRequest, WorkspaceResponse,
};
use loom_server::{InProcessBackend, InProcessConnection};

use crate::cli::args::{self, ApprovalPreset, Format, Parsed, Verb};
use crate::cli::client::Client;
use crate::cli::client::testing::{InProcessClient, Wrapped};
use crate::cli::exit;
use crate::cli::run_command;
use crate::cli::text;
use crate::cli::verbs;

/// The connection URL the cursor-file tests pretend to have connected to.
const URL: &str = "ws://cli.test.invalid/control";

/// What one CLI run printed and returned.
struct Outcome {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run<C: Client>(client: &mut C, arguments: &[&str]) -> Outcome {
    let arguments: Vec<String> = arguments.iter().map(|value| (*value).to_owned()).collect();
    run_owned(client, &arguments)
}

fn run_owned<C: Client>(client: &mut C, arguments: &[String]) -> Outcome {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime");
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = runtime.block_on(run_command(client, arguments, &mut out, &mut err));
    Outcome {
        code,
        stdout: String::from_utf8(out).expect("UTF-8 stdout"),
        stderr: String::from_utf8(err).expect("UTF-8 stderr"),
    }
}

/// The same run on a caller-owned runtime, for a test that is already inside
/// one and therefore cannot block on a second.
async fn run_async<C: Client>(client: &mut C, arguments: &[&str]) -> Outcome {
    let arguments: Vec<String> = arguments.iter().map(|value| (*value).to_owned()).collect();
    run_async_owned(client, &arguments).await
}

async fn run_async_owned<C: Client>(client: &mut C, arguments: &[String]) -> Outcome {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run_command(client, arguments, &mut out, &mut err).await;
    Outcome {
        code,
        stdout: String::from_utf8(out).expect("UTF-8 stdout"),
        stderr: String::from_utf8(err).expect("UTF-8 stderr"),
    }
}

/// A backend with one workspace and one session, plus a direct connection the
/// test can use to read the same state the CLI reads.
struct Fixture {
    backend: Arc<InProcessBackend>,
    control: InProcessConnection,
    workspace: loom_protocol::WorkspaceRecord,
    session: AgentSessionSnapshot,
}

fn fixture() -> Fixture {
    let backend = InProcessBackend::new();
    let control = backend.connect();
    negotiate(&control);
    let workspace = create_workspace(&control, "ctl test workspace");
    let session = create_session(&control, workspace.id, "ctl test session");
    Fixture {
        backend,
        control,
        workspace,
        session,
    }
}

fn negotiate(connection: &InProcessConnection) {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Control(
        ControlRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: test_capabilities(),
        },
    )));
    assert!(
        matches!(
            response.result,
            Ok(ServerResponse::Control(ControlResponse::Negotiated(_)))
        ),
        "negotiation failed: {:?}",
        response.result
    );
}

fn create_workspace(
    connection: &InProcessConnection,
    name: &str,
) -> loom_protocol::WorkspaceRecord {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: name.to_owned(),
            },
        )))
        .result
    {
        Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(record))) => record,
        other => panic!("unexpected workspace creation response: {other:?}"),
    }
}

fn create_session(
    connection: &InProcessConnection,
    workspace_id: WorkspaceId,
    name: &str,
) -> AgentSessionSnapshot {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id,
                name: name.to_owned(),
            },
        )))
        .result
    {
        Ok(ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot))) => snapshot,
        other => panic!("unexpected session creation response: {other:?}"),
    }
}

/// Starts one deterministic run and returns its id.
fn start_run(connection: &InProcessConnection, session_id: AgentSessionId) -> RunId {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRun {
                session_id,
                task: "make a small repository change and validate it".to_owned(),
                model: ModelId::new("deterministic/demo"),
                system_instructions: None,
                repository_instructions: None,
            },
        )))
        .result
    {
        Ok(ServerResponse::Run(RunResponse::AgentRunStarted(snapshot))) => snapshot.id,
        other => panic!("unexpected run start response: {other:?}"),
    }
}

/// Reads a session's journal straight from the backend.
fn journal(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
) -> Vec<ServerEventEnvelope> {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id: Some(session_id),
                workspace_id: None,
                after_sequence: None,
                stream_epoch: None,
                activity_detail: ActivityDetail::Full,
            },
        )))
        .result
    {
        Ok(ServerResponse::Events(EventsResponse::SessionEvents { events, .. })) => events,
        other => panic!("unexpected session events response: {other:?}"),
    }
}

/// Waits for the deterministic run to finish, then for the journal to stop
/// changing, and returns it. No sleep is used as a synchronisation primitive:
/// the settle loop only ends once two consecutive reads agree.
fn settled_journal(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
) -> Vec<ServerEventEnvelope> {
    let mut previous = journal(connection, session_id);
    let completed = |events: &[ServerEventEnvelope]| {
        events.iter().any(|envelope| {
            matches!(
                &envelope.event,
                ServerEvent::Agent {
                    event: AgentEvent::RunCompleted { .. }
                }
            )
        })
    };
    for _ in 0..600 {
        if completed(&previous) {
            let current = journal(connection, session_id);
            if current == previous {
                return current;
            }
            previous = current;
            continue;
        }
        std::thread::sleep(Duration::from_millis(5));
        previous = journal(connection, session_id);
    }
    panic!("the deterministic run did not settle");
}

fn session_snapshot(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
) -> AgentSessionSnapshot {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::GetAgentSession { session_id },
        )))
        .result
    {
        Ok(ServerResponse::Session(SessionResponse::AgentSession(snapshot))) => snapshot,
        other => panic!("unexpected session response: {other:?}"),
    }
}

/// The leading sequence number of one `loom-text/1` line.
fn sequence_of(line: &str) -> u64 {
    line.split(' ')
        .next()
        .and_then(|sequence| sequence.parse().ok())
        .unwrap_or_else(|| panic!("'{line}' does not start with a sequence"))
}

fn lines(stdout: &str) -> Vec<&str> {
    stdout.lines().collect()
}

fn temporary_path(suffix: &str) -> PathBuf {
    std::env::temp_dir().join(format!("loom-cli-{}-{suffix}", WorkspaceId::new()))
}

/// The capabilities a test's direct control connection negotiates: everything
/// the fixture needs to build state, which is a superset of what the CLI
/// negotiates for its read-only verbs.
fn test_capabilities() -> CapabilitySet {
    CapabilitySet::new([
        Capability::CreateAgentSession,
        Capability::ReadAgentSession,
        Capability::SubscribeSessionEvents,
        Capability::ManageWorkspaces,
        Capability::ManageSessionRepositories,
        Capability::ReadSessionFilesystem,
        Capability::WriteSessionFilesystem,
        Capability::ReadWorkerNodeStatus,
        Capability::JsonProtocol,
        Capability::StartAgentRun,
        Capability::ReadAgentRun,
        Capability::ConfigureApprovalPolicy,
        Capability::ControlAgentSession,
        Capability::ApproveAgentAction,
        Capability::ControlAgentRun,
        Capability::ReadVcsStatus,
        Capability::ReadVcsDiff,
        Capability::ReadSessionTaskEvidence,
        Capability::StartSessionTask,
    ])
}

// 1. Exact stdout and exit code for the read-only verbs.

#[test]
fn whoami_ls_get_and_doctor_print_exact_lines() {
    let fixture = fixture();
    let session_id = fixture.session.id;
    let workspace_id = fixture.workspace.id;
    let session_text = session_id.to_string();

    let mut client = InProcessClient::new(&fixture.backend);
    let whoami = run(&mut client, &["whoami"]);
    assert_eq!(whoami.code, exit::OK);
    assert_eq!(whoami.stdout, "participant operator\n");
    assert_eq!(whoami.stderr, "");

    let ls = run(&mut client, &["ls"]);
    assert_eq!(ls.code, exit::OK);
    assert_eq!(
        ls.stdout,
        format!("{} {}\n", workspace_id, fixture.workspace.name)
    );

    let sessions = run(
        &mut client,
        &["ls", "--workspace", &workspace_id.to_string()],
    );
    assert_eq!(sessions.code, exit::OK);
    assert_eq!(
        sessions.stdout,
        format!("{} idle {}\n", session_id, fixture.session.name)
    );

    let get = run(&mut client, &["get", &session_text]);
    assert_eq!(get.code, exit::OK);
    assert_eq!(
        get.stdout,
        format!("{} idle {}\n", session_id, fixture.session.name)
    );

    // A `get --metadata` of a session with no run prints only the session line.
    let metadata = run(&mut client, &["get", &session_text, "--metadata"]);
    assert_eq!(metadata.code, exit::OK);
    assert_eq!(metadata.stdout, get.stdout);

    let doctor = run(&mut client, &["doctor"]);
    assert_eq!(doctor.code, exit::OK);
    let node = worker_node(&fixture.control);
    let normalised = doctor.stdout.replace(&node.node_id, "NODE");
    assert_eq!(
        normalised,
        format!(
            "protocol {}.{}\ncapabilities 2 read_worker_node_status,json_protocol\nparticipant operator\n\
             node {} NODE\n",
            CURRENT_PROTOCOL_VERSION.major, CURRENT_PROTOCOL_VERSION.minor, node.name
        )
    );
}

#[test]
fn events_of_a_fresh_session_prints_its_creation_line() {
    let fixture = fixture();
    let events = journal(&fixture.control, fixture.session.id);
    assert_eq!(
        events.len(),
        1,
        "a fresh session journals only its creation"
    );
    let sequence = events[0].sequence;

    let mut client = InProcessClient::new(&fixture.backend);
    let outcome = run(
        &mut client,
        &["events", "--session", &fixture.session.id.to_string()],
    );
    assert_eq!(outcome.code, exit::OK);
    assert_eq!(outcome.stdout, format!("{sequence} session_created\n"));

    // `peek` sees the same single event.
    let peek = run(
        &mut client,
        &[
            "peek",
            "--session",
            &fixture.session.id.to_string(),
            "--lines",
            "5",
        ],
    );
    assert_eq!(peek.code, exit::OK);
    assert_eq!(peek.stdout, outcome.stdout);
}

fn worker_node(connection: &InProcessConnection) -> loom_protocol::WorkerNodeStatus {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::GetWorkerNodeStatus,
        )))
        .result
    {
        Ok(ServerResponse::Control(ControlResponse::WorkerNodeStatus(node))) => node,
        other => panic!("unexpected worker node response: {other:?}"),
    }
}

// 2. Byte-stable replay and text bounding.

#[test]
fn replaying_the_same_journal_produces_identical_bytes() {
    let fixture = fixture();
    start_run(&fixture.control, fixture.session.id);
    settled_journal(&fixture.control, fixture.session.id);
    let session = fixture.session.id.to_string();

    let mut client = InProcessClient::new(&fixture.backend);
    let first = run(&mut client, &["events", "--session", &session]);
    let second = run(&mut client, &["events", "--session", &session]);
    assert_eq!(first.code, exit::OK);
    assert_eq!(second.code, exit::OK);
    assert_eq!(first.stdout, second.stdout);
    assert!(first.stdout.lines().count() > 2, "the run journaled events");

    // The same events in JSON lines differ only in the envelope, not in the
    // projected kind and text.
    let jsonl = run(
        &mut client,
        &["events", "--session", &session, "--format", "jsonl"],
    );
    assert_eq!(jsonl.code, exit::OK);
    let text_lines = lines(&first.stdout);
    let json_lines = lines(&jsonl.stdout);
    assert_eq!(text_lines.len(), json_lines.len());
    for (text, object) in text_lines.iter().zip(json_lines.iter()) {
        let value: serde_json::Value = serde_json::from_str(object).expect("one JSON object");
        assert_eq!(value["seq"], sequence_of(text));
        let body = text.splitn(3, ' ').nth(2).unwrap_or("");
        assert_eq!(value["text"], body);
        let kind = text.split(' ').nth(1).expect("a kind word");
        assert_eq!(value["kind"], kind);
    }
}

#[test]
fn a_tool_output_over_the_budget_keeps_its_head_and_tail() {
    let fixture = fixture();
    let session_id = fixture.session.id;
    let chunk: String = "0123456789".repeat(30);
    assert_eq!(chunk.len(), 300);
    let envelope = ServerEventEnvelope {
        protocol_version: CURRENT_PROTOCOL_VERSION,
        sequence: EventSequence::new(7),
        session_id,
        event: ServerEvent::Agent {
            event: AgentEvent::ToolOutputChunk {
                run_id: RunId::new(),
                tool_call_id: ToolCallId::new(),
                chunk: chunk.clone(),
            },
        },
    };
    let scripted = envelope.clone();
    let mut client =
        Wrapped::new(InProcessClient::new(&fixture.backend)).after(move |request, response| {
            if !matches!(
                request,
                ClientRequest::Events(EventsRequest::GetSessionEvents { .. })
            ) {
                return;
            }
            if let Ok(ServerResponse::Events(EventsResponse::SessionEvents {
                stream_epoch, ..
            })) = &response.result
            {
                response.result = Ok(ServerResponse::Events(EventsResponse::SessionEvents {
                    events: vec![scripted.clone()],
                    stream_epoch: stream_epoch.clone(),
                    next_after_sequence: None,
                }));
            }
        });

    let outcome = run(
        &mut client,
        &[
            "events",
            "--session",
            &session_id.to_string(),
            "--max-bytes",
            "64",
        ],
    );
    assert_eq!(outcome.code, exit::OK);
    let line = outcome.stdout.lines().next().expect("one event line");
    assert!(line.starts_with("7 tool_output "), "{line}");
    let bounded = line
        .splitn(3, ' ')
        .nth(2)
        .and_then(|rest| rest.split_once(' '))
        .map(|(_, text)| text)
        .expect("a bounded body");
    let expected = format!(
        "{}[... 236 bytes omitted ...]{}",
        &chunk[..32],
        &chunk[268..]
    );
    assert_eq!(bounded, expected);
    assert_eq!(bounded.matches("bytes omitted").count(), 1);
}

#[test]
fn bounding_a_multibyte_text_lands_on_character_boundaries() {
    let text = "éééééééééé";
    let bounded = text::bound(text, 8);
    assert!(bounded.contains("[... "));
    assert!(bounded.starts_with("éé"));
    assert!(bounded.ends_with("éé"));
    assert_eq!(bounded, "éé[... 12 bytes omitted ...]éé");
    assert_eq!(text::bound("short", 64), "short");
}

// 3. Cursor resume, deduplication, and the snapshot resynchronisation.

/// Starts a deterministic run, waits for it to settle, and returns the journal
/// lines the CLI prints for the whole journal.
fn settled_lines(fixture: &Fixture) -> Vec<String> {
    start_run(&fixture.control, fixture.session.id);
    settled_journal(&fixture.control, fixture.session.id);
    let session = fixture.session.id.to_string();
    let mut client = InProcessClient::new(&fixture.backend);
    let full = run(
        &mut client,
        &["events", "--session", &session, "--connect", URL],
    );
    assert_eq!(full.code, exit::OK);
    let captured = lines(&full.stdout)
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(captured.len() > 4, "the run journaled several events");
    captured
}

fn cursor_record(url: &str, session: &str, after: u64, epoch: Option<&str>) -> String {
    serde_json::json!({
        "version": 1,
        "url": url,
        "session_id": session,
        "after": after,
        "stream_epoch": epoch,
    })
    .to_string()
}

#[test]
fn resuming_from_a_sequence_or_a_cursor_prints_exactly_the_suffix() {
    let fixture = fixture();
    let captured = settled_lines(&fixture);
    let session = fixture.session.id.to_string();
    let split = sequence_of(&captured[2]);
    let expected: Vec<&str> = captured
        .iter()
        .map(String::as_str)
        .filter(|line| sequence_of(line) > split)
        .collect();

    let mut client = InProcessClient::new(&fixture.backend);
    let suffix = run(
        &mut client,
        &[
            "events",
            "--session",
            &session,
            "--after",
            &split.to_string(),
            "--connect",
            URL,
        ],
    );
    assert_eq!(suffix.code, exit::OK);
    assert_eq!(lines(&suffix.stdout), expected);

    // A cursor at the same position prints the same suffix, then persists the
    // head so replaying it prints nothing.
    let cursor = temporary_path("cursor.json");
    std::fs::write(&cursor, cursor_record(URL, &session, split, None)).unwrap();
    let cursor_text = cursor.to_string_lossy().into_owned();
    let resumed = run(
        &mut client,
        &[
            "events",
            "--session",
            &session,
            "--cursor-file",
            &cursor_text,
            "--connect",
            URL,
        ],
    );
    assert_eq!(resumed.code, exit::OK);
    assert_eq!(resumed.stdout, suffix.stdout);

    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cursor).unwrap()).unwrap();
    let head = sequence_of(captured.last().unwrap());
    assert_eq!(record["version"], 1);
    assert_eq!(record["url"], URL);
    assert_eq!(record["session_id"], session.as_str());
    assert_eq!(record["after"], head);
    assert!(record["stream_epoch"].is_string());

    let replay = run(
        &mut client,
        &[
            "events",
            "--session",
            &session,
            "--cursor-file",
            &cursor_text,
            "--connect",
            URL,
        ],
    );
    assert_eq!(replay.code, exit::OK);
    assert_eq!(replay.stdout, "");
    assert_eq!(replay.stderr, "");

    // A cursor written for another URL is not this run's position.
    std::fs::write(
        &cursor,
        cursor_record("ws://elsewhere.invalid/control", &session, split, None),
    )
    .unwrap();
    let foreign = run(
        &mut client,
        &[
            "events",
            "--session",
            &session,
            "--cursor-file",
            &cursor_text,
            "--connect",
            URL,
        ],
    );
    assert_eq!(foreign.code, exit::OK);
    assert_eq!(foreign.stdout, captured.join("\n") + "\n");
    std::fs::remove_file(&cursor).ok();
}

#[test]
fn a_stale_cursor_resynchronises_without_reprinting_history() {
    let fixture = fixture();
    let captured = settled_lines(&fixture);
    let session = fixture.session.id.to_string();
    let split = sequence_of(&captured[2]);
    let expected: Vec<&str> = captured
        .iter()
        .map(String::as_str)
        .filter(|line| sequence_of(line) > split)
        .collect();

    let events = journal(&fixture.control, fixture.session.id);
    let head = events
        .last()
        .map(|envelope| envelope.sequence)
        .unwrap_or_default();
    let epoch = worker_node(&fixture.control).node_id;
    let snapshot = fixture.session.clone();
    let scripted = events.clone();
    let mut client =
        Wrapped::new(InProcessClient::new(&fixture.backend)).after(move |request, response| {
            if !matches!(
                request,
                ClientRequest::Events(EventsRequest::GetSessionEvents { .. })
            ) {
                return;
            }
            if let Ok(ServerResponse::Events(EventsResponse::SessionEvents {
                stream_epoch, ..
            })) = &response.result
            {
                // A worker whose retained window no longer reaches the cursor
                // answers with a snapshot instead of a batch.
                response.result = Ok(ServerResponse::Events(
                    EventsResponse::SessionEventsSnapshot {
                        session: snapshot.clone(),
                        events: scripted.clone(),
                        oldest_sequence: EventSequence::new(1),
                        latest_sequence: head,
                        stream_epoch: stream_epoch.clone(),
                    },
                ));
            }
        });

    let cursor = temporary_path("stale.json");
    std::fs::write(&cursor, cursor_record(URL, &session, split, Some(&epoch))).unwrap();
    let cursor_text = cursor.to_string_lossy().into_owned();
    let resumed = run(
        &mut client,
        &[
            "events",
            "--session",
            &session,
            "--cursor-file",
            &cursor_text,
            "--connect",
            URL,
        ],
    );
    assert_eq!(resumed.code, exit::OK);
    assert_eq!(lines(&resumed.stdout), expected);

    // The snapshot persisted the head, so a second snapshot prints nothing.
    let replay = run(
        &mut client,
        &[
            "events",
            "--session",
            &session,
            "--cursor-file",
            &cursor_text,
            "--connect",
            URL,
        ],
    );
    assert_eq!(replay.code, exit::OK);
    assert_eq!(replay.stdout, "");
    std::fs::remove_file(&cursor).ok();
}

#[test]
fn a_truncated_snapshot_reports_the_missing_range_instead_of_skipping_it() {
    let fixture = fixture();
    let captured = settled_lines(&fixture);
    let session = fixture.session.id.to_string();
    let events = journal(&fixture.control, fixture.session.id);
    assert!(events.len() > 4, "the run journaled several events");

    // The worker bounds a snapshot to its newest side, so this window starts
    // well above the cursor: everything between them is absent from the
    // response and cannot be paged back by a later snapshot.
    let window = events[events.len() - 2..].to_vec();
    let oldest = window[0].sequence;
    let latest = window[1].sequence;
    let resume_from = sequence_of(&captured[0]);
    assert!(oldest.value() > resume_from.saturating_add(1));
    let expected: Vec<&str> = captured
        .iter()
        .map(String::as_str)
        .filter(|line| {
            let sequence = sequence_of(line);
            sequence == oldest.value() || sequence == latest.value()
        })
        .collect();
    assert_eq!(expected.len(), 2, "the window carries exactly two events");

    let snapshot = fixture.session.clone();
    let scripted = window.clone();
    let mut client =
        Wrapped::new(InProcessClient::new(&fixture.backend)).after(move |request, response| {
            if !matches!(
                request,
                ClientRequest::Events(EventsRequest::GetSessionEvents { .. })
            ) {
                return;
            }
            if let Ok(ServerResponse::Events(EventsResponse::SessionEvents {
                stream_epoch, ..
            })) = &response.result
            {
                response.result = Ok(ServerResponse::Events(
                    EventsResponse::SessionEventsSnapshot {
                        session: snapshot.clone(),
                        events: scripted.clone(),
                        oldest_sequence: oldest,
                        latest_sequence: latest,
                        stream_epoch: stream_epoch.clone(),
                    },
                ));
            }
        });

    // Text mode prints the window, names the missing range on stderr, and moves
    // the cursor to the head of the window rather than pretending completeness.
    let cursor = temporary_path("truncated.json");
    std::fs::write(&cursor, cursor_record(URL, &session, resume_from, None)).unwrap();
    let cursor_text = cursor.to_string_lossy().into_owned();
    let outcome = run(
        &mut client,
        &[
            "events",
            "--session",
            &session,
            "--cursor-file",
            &cursor_text,
            "--connect",
            URL,
        ],
    );
    assert_eq!(outcome.code, exit::OK, "{}", outcome.stderr);
    assert_eq!(lines(&outcome.stdout), expected);
    assert!(
        outcome.stderr.contains(&format!(
            "warning gap {}..{}",
            resume_from + 1,
            oldest.value() - 1
        )),
        "the warning names the missing range: {}",
        outcome.stderr
    );
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cursor).unwrap()).unwrap();
    assert_eq!(record["after"], latest.value());

    // The `--json` envelope carries the same range as a field.
    std::fs::write(&cursor, cursor_record(URL, &session, resume_from, None)).unwrap();
    let envelope = run(
        &mut client,
        &[
            "events",
            "--session",
            &session,
            "--cursor-file",
            &cursor_text,
            "--json",
            "--connect",
            URL,
        ],
    );
    assert_eq!(envelope.code, exit::OK, "{}", envelope.stderr);
    let value: serde_json::Value = serde_json::from_str(envelope.stdout.trim()).unwrap();
    assert_eq!(value["data"]["gap"]["after"], resume_from);
    assert_eq!(value["data"]["gap"]["before"], oldest.value());
    assert_eq!(
        value["data"]["gap"]["missing"],
        oldest.value() - resume_from - 1
    );
    assert_eq!(value["data"]["events"].as_array().unwrap().len(), 2);

    std::fs::remove_file(&cursor).ok();
}

// 4. The read-only verbs change nothing.

#[test]
fn ls_get_events_and_peek_issue_no_mutation() {
    let fixture = fixture();
    let captured = settled_lines(&fixture);
    let session = fixture.session.id.to_string();
    let before_events = journal(&fixture.control, fixture.session.id);
    let before_session = session_snapshot(&fixture.control, fixture.session.id);

    {
        let mut client = InProcessClient::new(&fixture.backend);
        for arguments in [
            vec!["ls".to_owned()],
            vec!["get".to_owned(), session.clone()],
            vec!["events".to_owned(), "--session".to_owned(), session.clone()],
            vec!["peek".to_owned(), "--session".to_owned(), session.clone()],
        ] {
            let outcome = run_owned(&mut client, &arguments);
            assert_eq!(outcome.code, exit::OK, "{:?}", arguments);
        }
        let events = run_owned(
            &mut client,
            &["events".to_owned(), "--session".to_owned(), session.clone()],
        );
        assert_eq!(events.stdout, captured.join("\n") + "\n");
    }

    assert_eq!(before_events, journal(&fixture.control, fixture.session.id));
    assert_eq!(
        before_session,
        session_snapshot(&fixture.control, fixture.session.id)
    );
}

#[test]
fn a_read_only_scoped_token_can_read_but_cannot_start_a_run() {
    // A token scoped to one workspace and only the capabilities a read needs is
    // the read-only credential the contract promises to support. The same
    // client code runs the read verbs and is refused the one mutation.
    use loom_local::WebSocketTransport;
    use loom_server::{AuthTokenStore, AuthorizationScope, RemoteServer, RemoteServerConfig};

    let backend = InProcessBackend::new();
    let control = backend.connect();
    negotiate(&control);
    let workspace = create_workspace(&control, "read only workspace");
    let session = create_session(&control, workspace.id, "read only session");
    let session_text = session.id.to_string();

    let read_capabilities = CapabilitySet::new([
        Capability::ReadAgentSession,
        Capability::SubscribeSessionEvents,
        Capability::ManageWorkspaces,
        Capability::ReadWorkerNodeStatus,
        Capability::JsonProtocol,
    ]);
    let auth = Arc::new(AuthTokenStore::new());
    auth.insert(
        "read-only-token",
        AuthorizationScope::for_workspaces([workspace.id], read_capabilities),
    )
    .unwrap();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let server =
            RemoteServer::new(backend.clone(), auth, RemoteServerConfig::local_ephemeral())
                .bind()
                .await
                .unwrap();
        let transport = WebSocketTransport::new(server.websocket_url(), "read-only-token");
        let mut client = crate::cli::client::RemoteClient::new(transport.connect().await.unwrap());

        let ls = run_async(&mut client, &["ls"]).await;
        assert_eq!(ls.code, exit::OK, "{}", ls.stderr);
        assert_eq!(ls.stdout, format!("{} read only workspace\n", workspace.id));

        for arguments in [
            vec!["get".to_owned(), session_text.clone()],
            vec![
                "events".to_owned(),
                "--session".to_owned(),
                session_text.clone(),
            ],
            vec![
                "peek".to_owned(),
                "--session".to_owned(),
                session_text.clone(),
            ],
        ] {
            let outcome = run_async_owned(&mut client, &arguments).await;
            assert_eq!(outcome.code, exit::OK, "{arguments:?}: {}", outcome.stderr);
        }

        // The token does not carry the capabilities a run needs, so the write
        // side is refused and the read path above still worked.
        let started = run_async(
            &mut client,
            &[
                "run",
                "--workspace",
                &workspace.id.to_string(),
                "--task",
                "a task",
            ],
        )
        .await;
        assert_eq!(started.code, exit::BLOCKED);
        assert_eq!(started.stdout, "");
        assert!(
            started.stderr.contains("authorization_denied"),
            "{}",
            started.stderr
        );

        server.stop().await.unwrap();
    });
    backend.shutdown().unwrap();
}

// 5. The `--json` envelopes and the exit-code classes.

#[test]
fn each_verb_negotiates_only_the_capabilities_its_requests_need() {
    let workspace = WorkspaceId::new().to_string();
    let session = AgentSessionId::new().to_string();
    let run = RunId::new().to_string();
    let cases: Vec<(Vec<&str>, Vec<Capability>)> = vec![
        (vec!["whoami"], vec![Capability::JsonProtocol]),
        (
            vec!["ls"],
            vec![Capability::JsonProtocol, Capability::ReadAgentSession],
        ),
        (
            vec!["get", &session],
            vec![Capability::JsonProtocol, Capability::ReadAgentSession],
        ),
        (
            vec!["events", "--session", &session],
            vec![Capability::JsonProtocol, Capability::SubscribeSessionEvents],
        ),
        (
            vec!["peek", "--session", &session],
            vec![Capability::JsonProtocol, Capability::SubscribeSessionEvents],
        ),
        (
            vec!["doctor"],
            vec![Capability::JsonProtocol, Capability::ReadWorkerNodeStatus],
        ),
        (
            vec!["run", "--workspace", &workspace, "--task", "a task"],
            vec![
                Capability::JsonProtocol,
                Capability::ReadAgentSession,
                Capability::CreateAgentSession,
                Capability::ManageSessionRepositories,
                Capability::WriteSessionFilesystem,
                Capability::ConfigureApprovalPolicy,
                Capability::StartAgentRun,
            ],
        ),
        (
            vec![
                "run",
                "--workspace",
                &workspace,
                "--task",
                "a task",
                "--new",
            ],
            vec![
                Capability::JsonProtocol,
                Capability::CreateAgentSession,
                Capability::ManageSessionRepositories,
                Capability::WriteSessionFilesystem,
                Capability::ConfigureApprovalPolicy,
                Capability::StartAgentRun,
            ],
        ),
        (
            vec!["send", "--run", &run, "--text", "go"],
            vec![
                Capability::JsonProtocol,
                Capability::ControlAgentRun,
                Capability::ReadAgentRun,
            ],
        ),
        (
            vec!["answer", "--run", &run, "--text", "yes"],
            vec![
                Capability::JsonProtocol,
                Capability::ControlAgentRun,
                Capability::ReadAgentRun,
            ],
        ),
        (
            vec!["approve", "--run", &run, "--tool-call", &run],
            vec![
                Capability::JsonProtocol,
                Capability::ApproveAgentAction,
                Capability::ReadAgentRun,
            ],
        ),
        (
            vec!["deny", "--run", &run, "--tool-call", &run],
            vec![
                Capability::JsonProtocol,
                Capability::ApproveAgentAction,
                Capability::ReadAgentRun,
            ],
        ),
        (
            vec!["stop", "--run", &run],
            vec![
                Capability::JsonProtocol,
                Capability::ControlAgentRun,
                Capability::ReadAgentRun,
            ],
        ),
        (
            vec!["stop", "--run", &run, "--session", &session, "--archive"],
            vec![
                Capability::JsonProtocol,
                Capability::ControlAgentRun,
                Capability::ReadAgentRun,
                Capability::ControlAgentSession,
            ],
        ),
        (
            vec!["wait", "--session", &session],
            vec![
                Capability::JsonProtocol,
                Capability::ReadAgentSession,
                Capability::ReadAgentRun,
            ],
        ),
        (
            vec!["status", "--session", &session],
            vec![Capability::JsonProtocol, Capability::ReadAgentSession],
        ),
        (
            vec!["liveness", "--session", &session],
            vec![Capability::JsonProtocol, Capability::ReadAgentSession],
        ),
        (
            vec!["result", "--session", &session],
            vec![
                Capability::JsonProtocol,
                Capability::ReadSessionFilesystem,
                Capability::ReadVcsStatus,
                Capability::ReadVcsDiff,
                Capability::ReadSessionTaskEvidence,
            ],
        ),
    ];
    for (arguments, expected) in cases {
        let invocation = invocation(&arguments);
        assert_eq!(
            verbs::used_capabilities(&invocation.verb),
            CapabilitySet::new(expected),
            "{arguments:?}"
        );
    }
}

/// A stdout whose reader went away, as when `events` is piped into `head`.
struct ClosedPipe;

impl std::io::Write for ClosedPipe {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_closed_stdout_is_a_clean_stop() {
    let fixture = fixture();
    let mut client = InProcessClient::new(&fixture.backend);
    let arguments: Vec<String> = vec![
        "events".to_owned(),
        "--session".to_owned(),
        fixture.session.id.to_string(),
    ];
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut out = ClosedPipe;
    let mut err = Vec::new();
    let code = runtime.block_on(run_command(&mut client, &arguments, &mut out, &mut err));
    assert_eq!(code, exit::OK, "a closed pipe is not an error");
    assert!(err.is_empty(), "nothing is reported for a closed pipe");
}

#[test]
fn json_envelopes_and_error_exit_codes_are_pinned() {
    let fixture = fixture();
    let mut client = InProcessClient::new(&fixture.backend);

    let success = run(&mut client, &["whoami", "--json"]);
    assert_eq!(success.code, exit::OK);
    let envelope: serde_json::Value = serde_json::from_str(success.stdout.trim()).unwrap();
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["verb"], "whoami");
    assert_eq!(envelope["data"]["participant"], serde_json::Value::Null);
    assert_eq!(
        envelope["request_id"].as_str().map(str::len),
        Some(36),
        "a UUID request id"
    );

    let usage = run(&mut client, &["get", "--json"]);
    assert_eq!(usage.code, exit::USAGE);
    let envelope: serde_json::Value = serde_json::from_str(usage.stdout.trim()).unwrap();
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["verb"], "get");
    assert_eq!(envelope["request_id"], serde_json::Value::Null);
    assert_eq!(envelope["error"]["code"], "invalid_request");
    assert_eq!(envelope["error"]["retryable"], false);
    assert_eq!(envelope["error"]["exit_code"], 2);
    assert_eq!(usage.stderr, "");

    let missing_id = AgentSessionId::new().to_string();
    let missing = run(&mut client, &["get", &missing_id, "--json"]);
    assert_eq!(missing.code, exit::GONE);
    let envelope: serde_json::Value = serde_json::from_str(missing.stdout.trim()).unwrap();
    assert_eq!(envelope["error"]["code"], "not_found");
    assert_eq!(envelope["error"]["exit_code"], 5);
    assert_eq!(
        envelope["request_id"].as_str().map(str::len),
        Some(36),
        "the failing request id"
    );

    // A worker that speaks another protocol version is a mismatch.
    let mut incompatible =
        Wrapped::new(InProcessClient::new(&fixture.backend)).before(|envelope| {
            if let ClientRequest::Control(ControlRequest::Negotiate { client_version, .. }) =
                &mut envelope.request
            {
                *client_version = ProtocolVersion::new(99, 0);
            }
        });
    let mismatch = run(&mut incompatible, &["doctor", "--json"]);
    assert_eq!(mismatch.code, exit::MISMATCH);
    let envelope: serde_json::Value = serde_json::from_str(mismatch.stdout.trim()).unwrap();
    assert_eq!(envelope["verb"], "doctor");
    assert_eq!(envelope["error"]["code"], "unsupported_protocol");
    assert_eq!(envelope["error"]["exit_code"], 6);

    // A connection that never delivers the request is a transport failure.
    let mut offline = OfflineClient;
    let unreachable = run(&mut offline, &["whoami", "--json"]);
    assert_eq!(unreachable.code, exit::UNREACHABLE);
    let envelope: serde_json::Value = serde_json::from_str(unreachable.stdout.trim()).unwrap();
    assert_eq!(envelope["error"]["code"], "request_cancelled");
    assert_eq!(envelope["error"]["exit_code"], 7);
}

/// A client whose connection is already gone.
struct OfflineClient;

impl Client for OfflineClient {
    fn request<'a>(
        &'a mut self,
        _envelope: RequestEnvelope,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ResponseEnvelope, loom_core::LoomError>> + 'a>,
    > {
        Box::pin(async move {
            Err(loom_core::LoomError::new(
                loom_core::ErrorCode::RequestCancelled,
                "the connection is closed",
                true,
            ))
        })
    }
}

#[test]
fn every_exit_code_class_is_mapped() {
    use loom_core::ErrorCode::*;
    for code in [
        ApprovalRequired,
        CapabilityDenied,
        AuthorizationDenied,
        WorkspaceAccessDenied,
    ] {
        assert_eq!(exit::for_code(code), exit::BLOCKED, "{code}");
    }
    for code in [DeadlineExceeded, Backpressure] {
        assert_eq!(exit::for_code(code), exit::TIMEOUT, "{code}");
    }
    for code in [NotFound, InvalidState] {
        assert_eq!(exit::for_code(code), exit::GONE, "{code}");
    }
    for code in [
        UnsupportedProtocol,
        AuthenticationRequired,
        AuthenticationFailed,
    ] {
        assert_eq!(exit::for_code(code), exit::MISMATCH, "{code}");
    }
    for code in [Internal, InvalidRequest, Conflict, Persistence, Vcs] {
        assert_eq!(exit::for_code(code), exit::INTERNAL, "{code}");
    }
    assert_eq!(exit::OK, 0);
    assert_eq!(exit::USAGE, 2);
    assert_eq!(exit::UNREACHABLE, 7);
    assert_eq!(
        exit::for_connect_error(&loom_core::LoomError::new(
            AuthenticationFailed,
            "rejected",
            false
        )),
        exit::MISMATCH
    );
    assert_eq!(
        exit::for_connect_error(&loom_core::LoomError::new(
            loom_core::ErrorCode::InvalidRequest,
            "bad url",
            false
        )),
        exit::UNREACHABLE
    );
}

// 6. The token never reaches the command line or any output.

#[test]
fn following_a_session_stops_after_the_run_completes() {
    // The journal is settled first so the follow loop meets its stop condition
    // in the first batch: `--follow` has no iteration bound of its own, and this
    // test must never depend on how fast a run finishes.
    let fixture = fixture();
    let captured = settled_lines(&fixture);
    assert!(
        captured.iter().any(|line| line.contains(" run_completed ")),
        "the settled journal includes the completed run: {captured:?}"
    );
    let session = fixture.session.id.to_string();
    let mut client = InProcessClient::new(&fixture.backend);
    let followed = run(&mut client, &["events", "--session", &session, "--follow"]);
    assert_eq!(followed.code, exit::OK);
    assert_eq!(followed.stdout, captured.join("\n") + "\n");
}

#[test]
fn the_token_is_only_accepted_from_a_file() {
    let fixture = fixture();
    let secret = "s3cr3t-token-value";
    let mut client = InProcessClient::new(&fixture.backend);

    let rejected = run(&mut client, &["whoami", "--token", secret]);
    assert_eq!(rejected.code, exit::USAGE);
    assert!(
        rejected.stderr.contains("token must come from a file"),
        "{}",
        rejected.stderr
    );
    assert!(rejected.stderr.contains("--token-file"));

    let as_equals = run(&mut client, &[&format!("--token={secret}"), "whoami"]);
    assert_eq!(as_equals.code, exit::USAGE);

    let as_json = run(&mut client, &["whoami", "--json", "--token", secret]);
    assert_eq!(as_json.code, exit::USAGE);
    let envelope: serde_json::Value = serde_json::from_str(as_json.stdout.trim()).unwrap();
    assert_eq!(envelope["error"]["code"], "invalid_request");

    for output in [
        &rejected.stdout,
        &rejected.stderr,
        &as_equals.stdout,
        &as_equals.stderr,
        &as_json.stdout,
        &as_json.stderr,
    ] {
        assert!(!output.contains(secret), "the token leaked: {output}");
        assert!(!output.contains("WebSocketTransport"), "{output}");
        assert!(!output.contains("bearer_token"), "{output}");
    }
}

// 8. The control verbs: send, answer, approve, deny, stop, wait, status,
// liveness, result, and `run --new`.

/// Waits for the session's latest run to reach `wanted` and returns the
/// projection that carries it.
fn await_run_state(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
    wanted: AgentRunState,
) -> AgentSessionSnapshotProjection {
    for _ in 0..1200 {
        let projection = session_projection(connection, session_id);
        if let Some(active) = &projection.active_run
            && active.run.state == wanted
        {
            return projection;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the run never reached {wanted:?}");
}

/// Pins the auto-approval policy so the deterministic run finishes without a
/// person's decision.
fn pin_auto_policy(connection: &InProcessConnection, session_id: AgentSessionId) {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::SetSessionApprovalPolicy {
            session_id,
            policy: ApprovalPolicy::auto_approve(),
            auto_approve_actions: Some(true),
        },
    )));
    assert!(
        matches!(
            response.result,
            Ok(ServerResponse::Session(SessionResponse::ApprovalPolicy(_)))
        ),
        "approval policy: {:?}",
        response.result
    );
}

/// Pins the shipped default policy, so the deterministic run stops on its
/// first write and parks on an approval.
fn pin_default_policy(connection: &InProcessConnection, session_id: AgentSessionId) {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::SetSessionApprovalPolicy {
            session_id,
            policy: ApprovalPolicy::default(),
            auto_approve_actions: Some(false),
        },
    )));
    assert!(
        matches!(
            response.result,
            Ok(ServerResponse::Session(SessionResponse::ApprovalPolicy(_)))
        ),
        "approval policy: {:?}",
        response.result
    );
}

/// Polls the journal until `done` accepts one event, so a test never depends
/// on a fixed sleep to observe a run's progress.
fn await_event(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
    done: impl Fn(&ServerEvent) -> bool,
) {
    for _ in 0..1200 {
        if journal(connection, session_id)
            .iter()
            .any(|envelope| done(&envelope.event))
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the expected event never arrived");
}

fn json_envelope(outcome: &Outcome) -> serde_json::Value {
    serde_json::from_str(outcome.stdout.trim()).unwrap_or_else(|error| {
        panic!(
            "'{out}' is not one JSON envelope: {error}",
            out = outcome.stdout
        )
    })
}

#[test]
fn send_directs_a_run_and_retries_once_after_a_stale_revision() {
    let fixture = fixture();
    let session_id = fixture.session.id;
    pin_auto_policy(&fixture.control, session_id);
    let run_id = start_run(&fixture.control, session_id);
    await_run_state(&fixture.control, session_id, AgentRunState::Completed);

    // The first direction is deliberately stale, so the worker answers a
    // conflict and the client has to re-read and retry exactly once.
    let sends = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let hook = std::rc::Rc::clone(&sends);
    let mut client = Wrapped::new(InProcessClient::new(&fixture.backend)).before(move |envelope| {
        if let ClientRequest::Collaboration(CollaborationRequest::DirectAgent {
            expected, ..
        }) = &mut envelope.request
        {
            hook.set(hook.get() + 1);
            if hook.get() == 1 {
                expected.action_sequence += 1;
            }
        }
    });
    let run_text = run_id.to_string();
    let directed = run(
        &mut client,
        &["send", "--run", &run_text, "--text", "follow up"],
    );
    assert_eq!(directed.code, exit::OK, "{}", directed.stderr);
    assert_eq!(directed.stderr, "");
    assert!(
        directed.stdout.starts_with(&format!("sent {run_id} ")),
        "{}",
        directed.stdout
    );
    assert_eq!(sends.get(), 2, "one stale direction and one retry");

    // The direction reached the run and continues it.
    await_event(&fixture.control, session_id, |event| {
        matches!(
            event,
            ServerEvent::Agent {
                event: AgentEvent::UserMessage { text, .. }
            } if text.contains("follow up")
        )
    });
    // It is attributed to the token's participant, which for this operator
    // connection is the operator.
    await_event(&fixture.control, session_id, |event| {
        matches!(
            event,
            ServerEvent::Collaboration { entry }
                if entry.author == Author::Operator
                    && matches!(
                        &entry.kind,
                        CollaborationEntryKind::AgentDirection { text }
                            if text.contains("follow up")
                    )
        )
    });

    // The same verb in `--json` reports the action, the run, and the revision.
    let as_json = run(
        &mut client,
        &["send", "--json", "--run", &run_text, "--text", "again"],
    );
    assert_eq!(as_json.code, exit::OK, "{}", as_json.stderr);
    let envelope = json_envelope(&as_json);
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["verb"], "send");
    assert_eq!(envelope["data"]["action"], "send");
    assert_eq!(envelope["data"]["run"]["id"], run_text);
    assert!(envelope["data"]["revision"]["action_sequence"].is_number());
}

#[test]
fn approve_and_deny_resolve_the_pending_tool_call() {
    for (verb, printed) in [("approve", "approved"), ("deny", "denied")] {
        let fixture = fixture();
        let session_id = fixture.session.id;
        pin_default_policy(&fixture.control, session_id);
        let run_id = start_run(&fixture.control, session_id);
        let projection = await_run_state(
            &fixture.control,
            session_id,
            AgentRunState::AwaitingApproval,
        );
        let call = projection
            .active_run
            .as_ref()
            .and_then(|active| active.pending_approval.as_ref())
            .map(|call| call.id)
            .expect("the run is parked on a tool call");
        let mut client = InProcessClient::new(&fixture.backend);
        let run_text = run_id.to_string();
        let call_text = call.to_string();
        let arguments = [verb, "--run", &run_text, "--tool-call", &call_text];
        let arguments = if verb == "deny" {
            vec![
                arguments[0],
                arguments[1],
                arguments[2],
                arguments[3],
                arguments[4],
                "--reason",
                "not now",
            ]
        } else {
            arguments.to_vec()
        };
        let outcome = run(&mut client, &arguments);
        assert_eq!(outcome.code, exit::OK, "{}", outcome.stderr);
        assert!(
            outcome
                .stdout
                .starts_with(&format!("{printed} {call} {run_id} ")),
            "{}",
            outcome.stdout
        );

        // The worker recorded the decision for exactly this call.
        let approved = verb == "approve";
        await_event(&fixture.control, session_id, |event| {
            matches!(
                event,
                ServerEvent::Agent {
                    event: AgentEvent::ToolApprovalDecided {
                        tool_call_id,
                        decision,
                        ..
                    }
                } if *tool_call_id == call
                    && matches!(
                        decision,
                        loom_protocol::ApprovalDecision::Approved
                    ) == approved
            )
        });
    }
}

#[test]
fn approve_retries_once_after_a_stale_revision() {
    let fixture = fixture();
    let session_id = fixture.session.id;
    pin_default_policy(&fixture.control, session_id);
    let run_id = start_run(&fixture.control, session_id);
    let projection = await_run_state(
        &fixture.control,
        session_id,
        AgentRunState::AwaitingApproval,
    );
    let call = projection
        .active_run
        .as_ref()
        .and_then(|active| active.pending_approval.as_ref())
        .map(|call| call.id)
        .expect("the run is parked on a tool call");

    let controls = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let hook = std::rc::Rc::clone(&controls);
    let mut client = Wrapped::new(InProcessClient::new(&fixture.backend)).before(move |envelope| {
        if let ClientRequest::Collaboration(CollaborationRequest::ControlRun { expected, .. }) =
            &mut envelope.request
        {
            hook.set(hook.get() + 1);
            if hook.get() == 1 {
                expected.control_revision += 1;
            }
        }
    });
    let outcome = run(
        &mut client,
        &[
            "approve",
            "--run",
            &run_id.to_string(),
            "--tool-call",
            &call.to_string(),
        ],
    );
    assert_eq!(outcome.code, exit::OK, "{}", outcome.stderr);
    assert_eq!(controls.get(), 2, "one stale approval and one retry");
    await_event(&fixture.control, session_id, |event| {
        matches!(
            event,
            ServerEvent::Agent {
                event: AgentEvent::ToolApprovalDecided { tool_call_id, .. }
            } if *tool_call_id == call
        )
    });
}

#[test]
fn answer_reports_a_run_that_is_not_waiting_and_prints_an_answer_when_one_is() {
    let fixture = fixture();
    let session_id = fixture.session.id;
    pin_auto_policy(&fixture.control, session_id);
    let run_id = start_run(&fixture.control, session_id);
    await_run_state(&fixture.control, session_id, AgentRunState::Completed);
    let run_text = run_id.to_string();
    let mut client = InProcessClient::new(&fixture.backend);

    // A run that is not waiting refuses an answer with a stable code.
    let refused = run(
        &mut client,
        &["answer", "--run", &run_text, "--text", "yes"],
    );
    assert_eq!(refused.code, exit::GONE, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("invalid_state"),
        "{}",
        refused.stderr
    );

    // The verb itself is exercised against a real accepted run action: the
    // question path needs a run parked on a question, which the deterministic
    // provider never produces, so the request is routed to a direction after
    // asserting the answer request was what the verb built. The server-side
    // answer semantics are covered by the loom-server collaboration test.
    let mut rewired = Wrapped::new(InProcessClient::new(&fixture.backend)).before(|envelope| {
        if let ClientRequest::Collaboration(CollaborationRequest::AnswerAgentQuestion {
            run_id,
            expected,
            text,
        }) = envelope.request.clone()
        {
            envelope.request = ClientRequest::Collaboration(CollaborationRequest::DirectAgent {
                run_id,
                expected,
                text,
            });
        }
    });
    let answered = run(
        &mut rewired,
        &["answer", "--run", &run_text, "--text", "yes"],
    );
    assert_eq!(answered.code, exit::OK, "{}", answered.stderr);
    assert!(
        answered.stdout.starts_with(&format!("answered {run_id} ")),
        "{}",
        answered.stdout
    );
}

#[test]
fn stop_interrupts_archives_and_reports_a_finished_run() {
    let fixture = fixture();
    let session_id = fixture.session.id;
    pin_default_policy(&fixture.control, session_id);
    let run_id = start_run(&fixture.control, session_id);
    await_run_state(
        &fixture.control,
        session_id,
        AgentRunState::AwaitingApproval,
    );
    let mut client = InProcessClient::new(&fixture.backend);

    let stopped = run(&mut client, &["stop", "--run", &run_id.to_string()]);
    assert_eq!(stopped.code, exit::OK, "{}", stopped.stderr);
    assert_eq!(stopped.stdout, format!("stopped {run_id} cancelled\n"));

    // A run that already finished refuses to be stopped, and nothing changes.
    let again = run(&mut client, &["stop", "--run", &run_id.to_string()]);
    assert_eq!(again.code, exit::GONE, "{}", again.stderr);
    assert!(again.stderr.contains("invalid_state"), "{}", again.stderr);

    // `--archive` archives the session only after the interrupt succeeded.
    // A cancelled session accepts no second run, so this uses a fresh session.
    let other = create_session(&fixture.control, fixture.workspace.id, "archive me");
    pin_default_policy(&fixture.control, other.id);
    let second = start_run(&fixture.control, other.id);
    await_run_state(&fixture.control, other.id, AgentRunState::AwaitingApproval);
    let archived = run(
        &mut client,
        &[
            "stop",
            "--run",
            &second.to_string(),
            "--session",
            &other.id.to_string(),
            "--archive",
        ],
    );
    assert_eq!(archived.code, exit::OK, "{}", archived.stderr);
    assert_eq!(
        archived.stdout,
        format!("stopped {second} cancelled\narchived {}\n", other.id)
    );
    assert_eq!(
        session_snapshot(&fixture.control, other.id).state,
        AgentSessionState::Archived
    );
}

#[test]
fn stop_refuses_a_stale_revision_unless_the_flag_allows_a_retry() {
    let fixture = fixture();
    let session_id = fixture.session.id;
    pin_default_policy(&fixture.control, session_id);
    let run_id = start_run(&fixture.control, session_id);
    await_run_state(
        &fixture.control,
        session_id,
        AgentRunState::AwaitingApproval,
    );

    let controls = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let hook = std::rc::Rc::clone(&controls);
    let mut stale = Wrapped::new(InProcessClient::new(&fixture.backend)).before(move |envelope| {
        if let ClientRequest::Collaboration(CollaborationRequest::ControlRun { expected, .. }) =
            &mut envelope.request
        {
            hook.set(hook.get() + 1);
            expected.action_sequence += 1;
        }
    });
    let refused = run(&mut stale, &["stop", "--run", &run_id.to_string()]);
    assert_eq!(refused.code, exit::INTERNAL, "{}", refused.stderr);
    assert!(refused.stderr.contains("conflict"), "{}", refused.stderr);
    assert_eq!(controls.get(), 1, "stop does not retry by default");
    assert_eq!(
        session_projection(&fixture.control, session_id)
            .active_run
            .expect("the run is still there")
            .run
            .state,
        AgentRunState::AwaitingApproval,
        "a refused stop changes nothing"
    );

    let mut retrying = Wrapped::new(InProcessClient::new(&fixture.backend)).before({
        let controls = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let hook = std::rc::Rc::clone(&controls);
        move |envelope| {
            if let ClientRequest::Collaboration(CollaborationRequest::ControlRun {
                expected, ..
            }) = &mut envelope.request
            {
                hook.set(hook.get() + 1);
                if hook.get() == 1 {
                    expected.action_sequence += 1;
                }
            }
        }
    });
    let stopped = run(
        &mut retrying,
        &["stop", "--run", &run_id.to_string(), "--retry-on-conflict"],
    );
    assert_eq!(stopped.code, exit::OK, "{}", stopped.stderr);
    assert_eq!(stopped.stdout, format!("stopped {run_id} cancelled\n"));
}

#[test]
fn every_control_verb_reports_a_json_envelope() {
    for (verb, action) in [("approve", "approve"), ("deny", "deny")] {
        let fixture = fixture();
        let session_id = fixture.session.id;
        pin_default_policy(&fixture.control, session_id);
        let run_id = start_run(&fixture.control, session_id);
        let projection = await_run_state(
            &fixture.control,
            session_id,
            AgentRunState::AwaitingApproval,
        );
        let call = projection
            .active_run
            .as_ref()
            .and_then(|active| active.pending_approval.as_ref())
            .map(|call| call.id)
            .expect("the run is parked on a tool call");
        let run_text = run_id.to_string();
        let call_text = call.to_string();
        let mut client = InProcessClient::new(&fixture.backend);
        let outcome = run(
            &mut client,
            &[
                verb,
                "--json",
                "--run",
                &run_text,
                "--tool-call",
                &call_text,
            ],
        );
        assert_eq!(outcome.code, exit::OK, "{}", outcome.stderr);
        let envelope = json_envelope(&outcome);
        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["verb"], verb);
        assert_eq!(envelope["data"]["action"], action);
        assert_eq!(envelope["data"]["tool_call_id"], call_text.as_str());
        assert_eq!(envelope["data"]["run"]["id"], run_text.as_str());
    }

    let stop_fixture = fixture();
    let stop_session = stop_fixture.session.id;
    pin_default_policy(&stop_fixture.control, stop_session);
    let stop_run = start_run(&stop_fixture.control, stop_session);
    await_run_state(
        &stop_fixture.control,
        stop_session,
        AgentRunState::AwaitingApproval,
    );
    let mut stop_client = InProcessClient::new(&stop_fixture.backend);
    let stopped = run(
        &mut stop_client,
        &["stop", "--json", "--run", &stop_run.to_string()],
    );
    assert_eq!(stopped.code, exit::OK, "{}", stopped.stderr);
    let envelope = json_envelope(&stopped);
    assert_eq!(envelope["verb"], "stop");
    assert_eq!(envelope["data"]["action"], "stop");
    assert_eq!(envelope["data"]["run"]["state"], "cancelled");
    assert_eq!(envelope["data"]["archived"], serde_json::Value::Null);

    // `answer` needs a run parked on a question, which the deterministic
    // provider never produces; route its request to a direction so the
    // envelope path is exercised against a real accepted action.
    let answer_fixture = fixture();
    let answer_session = answer_fixture.session.id;
    pin_auto_policy(&answer_fixture.control, answer_session);
    let answer_run = start_run(&answer_fixture.control, answer_session);
    await_run_state(
        &answer_fixture.control,
        answer_session,
        AgentRunState::Completed,
    );
    let mut rewired =
        Wrapped::new(InProcessClient::new(&answer_fixture.backend)).before(|envelope| {
            if let ClientRequest::Collaboration(CollaborationRequest::AnswerAgentQuestion {
                run_id,
                expected,
                text,
            }) = envelope.request.clone()
            {
                envelope.request =
                    ClientRequest::Collaboration(CollaborationRequest::DirectAgent {
                        run_id,
                        expected,
                        text,
                    });
            }
        });
    let answered = run(
        &mut rewired,
        &[
            "answer",
            "--json",
            "--run",
            &answer_run.to_string(),
            "--text",
            "yes",
        ],
    );
    assert_eq!(answered.code, exit::OK, "{}", answered.stderr);
    let envelope = json_envelope(&answered);
    assert_eq!(envelope["verb"], "answer");
    assert_eq!(envelope["data"]["action"], "answer");
}

// 7. The `run` verb: the session it reuses or creates, the inputs it attaches,
// the policy it pins, and the run start it makes replayable.

/// A backend with one workspace and no session, which is the state a first
/// `run` meets.
struct EmptyFixture {
    backend: Arc<InProcessBackend>,
    control: InProcessConnection,
    workspace: loom_protocol::WorkspaceRecord,
}

fn empty_fixture() -> EmptyFixture {
    let backend = InProcessBackend::new();
    let control = backend.connect();
    negotiate(&control);
    let workspace = create_workspace(&control, "ctl run workspace");
    EmptyFixture {
        backend,
        control,
        workspace,
    }
}

/// The workspace's live sessions, exactly as `run` resolves them.
fn live_sessions(
    connection: &InProcessConnection,
    workspace: WorkspaceId,
) -> Vec<AgentSessionSnapshot> {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::ListWorkspaceSessions {
                workspace_id: workspace,
                include_archived: false,
            },
        )))
        .result
    {
        Ok(ServerResponse::Session(SessionResponse::AgentSessions { sessions })) => sessions,
        other => panic!("unexpected workspace sessions response: {other:?}"),
    }
}

fn run_snapshot(connection: &InProcessConnection, run_id: RunId) -> AgentRunSnapshot {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::GetAgentRun { run_id },
        )))
        .result
    {
        Ok(ServerResponse::Run(RunResponse::AgentRun(snapshot))) => snapshot,
        other => panic!("unexpected agent run response: {other:?}"),
    }
}

fn session_projection(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
) -> AgentSessionSnapshotProjection {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::GetAgentSessionSnapshotMetadata {
                session_id,
                activity_detail: ActivityDetail::Full,
            },
        )))
        .result
    {
        Ok(ServerResponse::Session(SessionResponse::AgentSessionSnapshot(projection))) => {
            *projection
        }
        other => panic!("unexpected session metadata response: {other:?}"),
    }
}

fn session_directories(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
) -> Vec<SessionDirectory> {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::ListSessionDirectories { session_id },
        )))
        .result
    {
        Ok(ServerResponse::Filesystem(FilesystemResponse::SessionDirectories { directories })) => {
            directories
        }
        other => panic!("unexpected session directories response: {other:?}"),
    }
}

fn session_repositories(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
) -> Vec<SessionRepository> {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::ListSessionRepositories { session_id },
        )))
        .result
    {
        Ok(ServerResponse::Repository(RepositoryResponse::SessionRepositories {
            repositories,
        })) => repositories,
        other => panic!("unexpected session repositories response: {other:?}"),
    }
}

/// The ids of the runs a session's journal recorded as started.
fn started_run_ids(connection: &InProcessConnection, session_id: AgentSessionId) -> Vec<RunId> {
    journal(connection, session_id)
        .iter()
        .filter_map(|envelope| match &envelope.event {
            ServerEvent::Agent {
                event: AgentEvent::RunStarted { snapshot },
            } => Some(snapshot.id),
            _ => None,
        })
        .collect()
}

/// The run id and state of the second line `run` prints.
fn printed_run(stdout: &str) -> (RunId, String) {
    let mut words = stdout
        .lines()
        .nth(1)
        .unwrap_or_else(|| panic!("'run' printed '{stdout}'"))
        .split(' ');
    assert_eq!(words.next(), Some("run"));
    let run_id = words
        .next()
        .and_then(|id| id.parse::<RunId>().ok())
        .unwrap_or_else(|| panic!("'run' printed no run id in '{stdout}'"));
    let state = words
        .next()
        .unwrap_or_else(|| panic!("'run' printed no run state in '{stdout}'"))
        .to_owned();
    (run_id, state)
}

#[test]
fn run_prints_the_session_and_the_started_run() {
    let fixture = empty_fixture();
    let workspace = fixture.workspace.id.to_string();
    let request_id = RequestId::new().to_string();
    let mut client = InProcessClient::new(&fixture.backend);

    let outcome = run(
        &mut client,
        &[
            "run",
            "--workspace",
            &workspace,
            "--task",
            "say hello",
            "--request-id",
            &request_id,
        ],
    );
    assert_eq!(outcome.code, exit::OK, "{}", outcome.stderr);
    assert_eq!(outcome.stderr, "");

    // The session this invocation created is the one it printed, and its name
    // is derived from the task.
    let sessions = live_sessions(&fixture.control, fixture.workspace.id);
    assert_eq!(sessions.len(), 1);
    let session = &sessions[0];
    assert_eq!(session.name, "say hello");
    let (run_id, state) = printed_run(&outcome.stdout);
    assert_eq!(state, "executing");
    assert_eq!(
        outcome.stdout,
        format!("session {} say hello\nrun {run_id} executing\n", session.id),
        "the whole output is the two contract lines"
    );

    // The printed run is the run the worker holds for that session.
    let snapshot = run_snapshot(&fixture.control, run_id);
    assert_eq!(snapshot.id, run_id);
    assert_eq!(snapshot.session_id, session.id);
    assert_eq!(started_run_ids(&fixture.control, session.id), vec![run_id]);

    // The default policy parks the run on its first write, which is a stable
    // state to shut down from.
    fixture.backend.shutdown().unwrap();
}

#[test]
fn a_replayed_request_id_returns_the_same_run_and_reuses_the_session() {
    let fixture = empty_fixture();
    let workspace = fixture.workspace.id.to_string();
    let request_id = RequestId::new().to_string();
    let arguments = [
        "run",
        "--workspace",
        &workspace,
        "--task",
        "say hello",
        "--request-id",
        &request_id,
    ];
    let mut client = InProcessClient::new(&fixture.backend);

    let first = run(&mut client, &arguments);
    assert_eq!(first.code, exit::OK, "{}", first.stderr);
    let replay = run(&mut client, &arguments);
    assert_eq!(replay.code, exit::OK, "{}", replay.stderr);
    assert_eq!(
        replay.stdout, first.stdout,
        "the replay reports the same run"
    );

    let sessions = live_sessions(&fixture.control, fixture.workspace.id);
    assert_eq!(sessions.len(), 1, "a replay does not create a session");
    let session_id = sessions[0].id;
    let started = started_run_ids(&fixture.control, session_id);
    assert_eq!(started.len(), 1, "the run start is idempotent: {started:?}");
    assert_eq!(started[0], printed_run(&replay.stdout).0);

    // Without `--request-id` the session is still reused. A second run cannot
    // start in it: the worker keeps a session to one run and refuses the next
    // with `invalid_state` until the session is reused for a fresh one, which
    // is why the verb resolves the session by name instead of starting blindly.
    let fresh = run(
        &mut client,
        &["run", "--workspace", &workspace, "--task", "say hello"],
    );
    assert_eq!(fresh.code, exit::GONE);
    assert_eq!(fresh.stdout, "");
    assert!(fresh.stderr.contains("invalid_state"), "{}", fresh.stderr);
    assert_eq!(
        live_sessions(&fixture.control, fixture.workspace.id).len(),
        1,
        "the reused session is the only one"
    );
    assert_eq!(started_run_ids(&fixture.control, session_id), started);

    fixture.backend.shutdown().unwrap();
}

#[test]
fn an_invocation_with_the_same_name_reuses_the_session() {
    let fixture = empty_fixture();
    let workspace = fixture.workspace.id.to_string();
    let mut client = InProcessClient::new(&fixture.backend);

    // The first invocation names a model this worker does not offer, so it
    // fails after creating the session and leaves an idle one behind.
    let failed = run(
        &mut client,
        &[
            "run",
            "--workspace",
            &workspace,
            "--name",
            "shared session",
            "--task",
            "first task",
            "--model",
            "deterministic/absent-model",
        ],
    );
    assert_eq!(failed.code, exit::INTERNAL, "{}", failed.stderr);
    assert_eq!(failed.stdout, "");
    let created = live_sessions(&fixture.control, fixture.workspace.id);
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].name, "shared session");
    assert_eq!(created[0].state, AgentSessionState::Idle);

    // A second invocation with a different task and the same name reuses it.
    let second = run(
        &mut client,
        &[
            "run",
            "--workspace",
            &workspace,
            "--name",
            "shared session",
            "--task",
            "second task",
        ],
    );
    assert_eq!(second.code, exit::OK, "{}", second.stderr);
    let sessions = live_sessions(&fixture.control, fixture.workspace.id);
    assert_eq!(
        sessions.len(),
        1,
        "the session was reused, not created twice"
    );
    assert_eq!(sessions[0].id, created[0].id);
    assert!(
        second
            .stdout
            .starts_with(&format!("session {} shared session\n", created[0].id)),
        "{}",
        second.stdout
    );
    let (run_id, _) = printed_run(&second.stdout);
    assert_eq!(run_snapshot(&fixture.control, run_id).task, "second task");

    fixture.backend.shutdown().unwrap();
}

#[test]
fn a_directory_is_attached_by_a_fresh_session_and_not_twice() {
    let fixture = empty_fixture();
    let workspace = fixture.workspace.id.to_string();
    let directory = temporary_path("attach-directory");
    std::fs::create_dir_all(&directory).unwrap();
    let source = directory.to_string_lossy().into_owned();
    let path = directory
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let request_id = RequestId::new().to_string();
    let arguments = [
        "run",
        "--workspace",
        &workspace,
        "--task",
        "mount a directory",
        "--as-directory",
        &source,
        "--request-id",
        &request_id,
    ];
    let mut client = InProcessClient::new(&fixture.backend);

    let attached = run(&mut client, &arguments);
    assert_eq!(attached.code, exit::OK, "{}", attached.stderr);
    let sessions = live_sessions(&fixture.control, fixture.workspace.id);
    assert_eq!(sessions.len(), 1);
    let directories = session_directories(&fixture.control, sessions[0].id);
    assert_eq!(directories.len(), 1);
    assert_eq!(
        directories[0].path, path,
        "--path defaults to the source name"
    );

    // The replay reuses the session and therefore attaches nothing, which is
    // what makes it succeed rather than collide with the mounted directory.
    let replay = run(&mut client, &arguments);
    assert_eq!(replay.code, exit::OK, "{}", replay.stderr);
    assert_eq!(replay.stdout, attached.stdout);
    assert_eq!(
        session_directories(&fixture.control, sessions[0].id).len(),
        1,
        "a replay does not attach a second directory"
    );

    fixture.backend.shutdown().unwrap();
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn a_repository_is_attached_by_a_fresh_session_and_not_twice() {
    let fixture = empty_fixture();
    let workspace = fixture.workspace.id.to_string();
    let repository = temporary_path("attach-repository");
    std::fs::create_dir_all(&repository).unwrap();
    match std::process::Command::new("git")
        .arg("init")
        .arg(&repository)
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => panic!(
            "git init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) => {
            println!("skipping: git init could not be spawned: {error}");
            std::fs::remove_dir_all(&repository).ok();
            return;
        }
    }
    let source = repository.to_string_lossy().into_owned();
    let path = repository
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let request_id = RequestId::new().to_string();
    let arguments = [
        "run",
        "--workspace",
        &workspace,
        "--task",
        "clone a repository",
        "--repo",
        &source,
        "--request-id",
        &request_id,
    ];
    let mut client = InProcessClient::new(&fixture.backend);

    let attached = run(&mut client, &arguments);
    assert_eq!(attached.code, exit::OK, "{}", attached.stderr);
    let sessions = live_sessions(&fixture.control, fixture.workspace.id);
    assert_eq!(sessions.len(), 1);
    let repositories = session_repositories(&fixture.control, sessions[0].id);
    assert_eq!(repositories.len(), 1);
    assert_eq!(repositories[0].path, path);

    let replay = run(&mut client, &arguments);
    assert_eq!(replay.code, exit::OK, "{}", replay.stderr);
    assert_eq!(replay.stdout, attached.stdout);
    assert_eq!(
        session_repositories(&fixture.control, sessions[0].id).len(),
        1,
        "a replay does not attach a second repository"
    );

    fixture.backend.shutdown().unwrap();
    std::fs::remove_dir_all(&repository).ok();
}

#[test]
fn the_approval_policy_flag_pins_the_session_policy() {
    let fixture = empty_fixture();
    let workspace = fixture.workspace.id.to_string();
    let mut client = InProcessClient::new(&fixture.backend);

    let auto = run(
        &mut client,
        &[
            "run",
            "--workspace",
            &workspace,
            "--task",
            "auto task",
            "--approval-policy",
            "auto",
        ],
    );
    assert_eq!(auto.code, exit::OK, "{}", auto.stderr);
    let automatic = session_projection(&fixture.control, printed_session(&auto.stdout));
    assert_eq!(automatic.approval_policy, ApprovalPolicy::auto_approve());
    assert!(automatic.auto_approve_actions);

    let default = run(
        &mut client,
        &["run", "--workspace", &workspace, "--task", "default task"],
    );
    assert_eq!(default.code, exit::OK, "{}", default.stderr);
    let pinned = session_projection(&fixture.control, printed_session(&default.stdout));
    assert_eq!(pinned.approval_policy, ApprovalPolicy::default());
    assert!(!pinned.auto_approve_actions);

    fixture.backend.shutdown().unwrap();
}

#[test]
fn a_reused_session_keeps_its_approval_policy_unless_the_flag_is_given() {
    let fixture = empty_fixture();
    let workspace = fixture.workspace.id.to_string();
    let mut client = InProcessClient::new(&fixture.backend);

    // The first invocation creates the session with the auto preset but names a
    // model the worker does not offer, so the start fails and the session stays
    // idle with the policy it pinned.
    let created = run(
        &mut client,
        &[
            "run",
            "--workspace",
            &workspace,
            "--name",
            "policy session",
            "--task",
            "first task",
            "--approval-policy",
            "auto",
            "--model",
            "deterministic/absent-model",
        ],
    );
    assert_eq!(created.code, exit::INTERNAL, "{}", created.stderr);
    let sessions = live_sessions(&fixture.control, fixture.workspace.id);
    assert_eq!(sessions.len(), 1);
    let session_id = sessions[0].id;
    assert_eq!(
        session_projection(&fixture.control, session_id).approval_policy,
        ApprovalPolicy::auto_approve()
    );

    // A plain reuse must not reset the policy a person chose.
    let reused = run(
        &mut client,
        &[
            "run",
            "--workspace",
            &workspace,
            "--name",
            "policy session",
            "--task",
            "second task",
        ],
    );
    assert_eq!(reused.code, exit::OK, "{}", reused.stderr);
    assert_eq!(
        session_projection(&fixture.control, session_id).approval_policy,
        ApprovalPolicy::auto_approve(),
        "a plain reuse keeps the stored policy"
    );

    // An explicit reuse does change it: create an idle session with the default
    // preset, then ask for auto while reusing it.
    let created_default = run(
        &mut client,
        &[
            "run",
            "--workspace",
            &workspace,
            "--name",
            "default session",
            "--task",
            "first task",
            "--model",
            "deterministic/absent-model",
        ],
    );
    assert_eq!(created_default.code, exit::INTERNAL);
    let default_session = live_sessions(&fixture.control, fixture.workspace.id)
        .into_iter()
        .find(|session| session.name == "default session")
        .expect("the default session was created");
    assert_eq!(
        session_projection(&fixture.control, default_session.id).approval_policy,
        ApprovalPolicy::default()
    );
    let changed = run(
        &mut client,
        &[
            "run",
            "--workspace",
            &workspace,
            "--name",
            "default session",
            "--task",
            "second task",
            "--approval-policy",
            "auto",
        ],
    );
    assert_eq!(changed.code, exit::OK, "{}", changed.stderr);
    assert_eq!(
        session_projection(&fixture.control, default_session.id).approval_policy,
        ApprovalPolicy::auto_approve(),
        "an explicit preset still applies to a reused session"
    );

    fixture.backend.shutdown().unwrap();
}

/// The session id the first line `run` prints names.
fn printed_session(stdout: &str) -> AgentSessionId {
    let mut words = stdout
        .lines()
        .next()
        .unwrap_or_else(|| panic!("'run' printed '{stdout}'"))
        .split(' ');
    assert_eq!(words.next(), Some("session"));
    words
        .next()
        .and_then(|id| id.parse::<AgentSessionId>().ok())
        .unwrap_or_else(|| panic!("'run' printed no session id in '{stdout}'"))
}

#[test]
fn a_task_file_supplies_the_task_text() {
    let fixture = empty_fixture();
    let workspace = fixture.workspace.id.to_string();
    let task_file = temporary_path("task-file.md");
    std::fs::write(&task_file, "audit the build\n").unwrap();
    let task_text = task_file.to_string_lossy().into_owned();
    let mut client = InProcessClient::new(&fixture.backend);

    let outcome = run(
        &mut client,
        &["run", "--workspace", &workspace, "--task-file", &task_text],
    );
    assert_eq!(outcome.code, exit::OK, "{}", outcome.stderr);
    let (run_id, _) = printed_run(&outcome.stdout);
    assert_eq!(
        run_snapshot(&fixture.control, run_id).task,
        "audit the build",
        "the task is the file's text without its trailing newline"
    );
    let sessions = live_sessions(&fixture.control, fixture.workspace.id);
    assert_eq!(sessions[0].name, "audit the build");

    fixture.backend.shutdown().unwrap();
    std::fs::remove_file(&task_file).ok();
}

#[test]
fn run_reports_conflicting_flags_an_invalid_request_id_and_an_unknown_workspace() {
    let fixture = empty_fixture();
    let workspace = fixture.workspace.id.to_string();
    let task_file = temporary_path("conflicting-task.md");
    std::fs::write(&task_file, "a task").unwrap();
    let task_text = task_file.to_string_lossy().into_owned();
    let directory = temporary_path("conflicting-directory");
    std::fs::create_dir_all(&directory).unwrap();
    let directory_text = directory.to_string_lossy().into_owned();
    let mut client = InProcessClient::new(&fixture.backend);

    for arguments in [
        vec![
            "run",
            "--workspace",
            workspace.as_str(),
            "--task",
            "a task",
            "--task-file",
            task_text.as_str(),
        ],
        vec![
            "run",
            "--workspace",
            workspace.as_str(),
            "--task",
            "a task",
            "--repo",
            directory_text.as_str(),
            "--as-directory",
            directory_text.as_str(),
        ],
        vec![
            "run",
            "--workspace",
            workspace.as_str(),
            "--task",
            "a task",
            "--request-id",
            "not-a-uuid",
        ],
        vec!["run", "--workspace", workspace.as_str()],
        vec![
            "run",
            "--workspace",
            workspace.as_str(),
            "--task",
            "a task",
            "--revision",
            "main",
            "--as-directory",
            directory_text.as_str(),
        ],
    ] {
        let outcome = run_owned(&mut client, &owned(&arguments));
        assert_eq!(outcome.code, exit::USAGE, "{arguments:?}");
        assert_eq!(outcome.stdout, "", "{arguments:?}");
        assert!(!outcome.stderr.is_empty(), "{arguments:?}");
    }
    assert!(
        live_sessions(&fixture.control, fixture.workspace.id).is_empty(),
        "a usage error creates no session"
    );

    // A workspace id the worker does not know is `not_found`.
    let missing = WorkspaceId::new().to_string();
    let outcome = run(
        &mut client,
        &["run", "--workspace", &missing, "--task", "a task"],
    );
    assert_eq!(outcome.code, exit::GONE);
    assert_eq!(outcome.stdout, "");
    assert!(outcome.stderr.contains("not_found"), "{}", outcome.stderr);

    fixture.backend.shutdown().unwrap();
    std::fs::remove_file(&task_file).ok();
    std::fs::remove_dir_all(&directory).ok();
}

fn owned(arguments: &[&str]) -> Vec<String> {
    arguments.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn the_run_json_envelope_carries_the_run_start_request_id() {
    let fixture = empty_fixture();
    let workspace = fixture.workspace.id.to_string();
    let request_id = RequestId::new().to_string();
    let mut client = InProcessClient::new(&fixture.backend);

    let outcome = run(
        &mut client,
        &[
            "run",
            "--workspace",
            &workspace,
            "--task",
            "say hello",
            "--request-id",
            &request_id,
            "--json",
        ],
    );
    assert_eq!(outcome.code, exit::OK, "{}", outcome.stderr);
    let envelope: serde_json::Value = serde_json::from_str(outcome.stdout.trim()).unwrap();
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["verb"], "run");
    assert_eq!(envelope["request_id"], request_id.as_str());
    assert_eq!(envelope["data"]["request_id"], request_id.as_str());
    assert_eq!(envelope["data"]["created"], true);
    assert_eq!(envelope["data"]["session"]["name"], "say hello");
    assert_eq!(
        envelope["data"]["run"]["state"], "executing",
        "the state is the snake_case name the text line uses"
    );

    let session_id: AgentSessionId = envelope["data"]["session"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let run_id: RunId = envelope["data"]["run"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let snapshot = run_snapshot(&fixture.control, run_id);
    assert_eq!(snapshot.session_id, session_id);
    assert_eq!(snapshot.id, run_id);

    fixture.backend.shutdown().unwrap();
}

// 8. The parser.

fn parse(arguments: &[&str]) -> Result<Parsed, String> {
    let arguments: Vec<String> = arguments.iter().map(|value| (*value).to_owned()).collect();
    args::parse(&arguments)
}

fn invocation(arguments: &[&str]) -> args::Invocation {
    match parse(arguments) {
        Ok(Parsed::Run(invocation)) => invocation,
        Ok(Parsed::Help) => panic!("{arguments:?} unexpectedly parsed as help"),
        Err(message) => panic!("{arguments:?} was rejected: {message}"),
    }
}

fn parse_error(arguments: &[&str]) -> String {
    match parse(arguments) {
        Ok(_) => panic!("{arguments:?} was unexpectedly accepted"),
        Err(message) => message,
    }
}

#[test]
fn the_parser_accepts_spaced_and_equals_forms_anywhere() {
    let session = AgentSessionId::new().to_string();
    let spaced = invocation(&[
        "--connect",
        URL,
        "events",
        "--session",
        &session,
        "--after",
        "7",
        "--format",
        "jsonl",
        "--max-bytes",
        "128",
        "--follow",
        "--cursor-file",
        "/tmp/ctl-cursor",
        "--json",
    ]);
    let equals = invocation(&[
        "events",
        "--session",
        &session,
        "--after=7",
        "--format=jsonl",
        "--max-bytes=128",
        "--follow",
        "--cursor-file=/tmp/ctl-cursor",
        "--json",
        &format!("--connect={URL}"),
    ]);
    for parsed in [&spaced, &equals] {
        assert_eq!(parsed.globals.connect.as_deref(), Some(URL));
        assert!(parsed.globals.json);
        let Verb::Events(events) = &parsed.verb else {
            panic!("events did not parse as the events verb");
        };
        assert_eq!(events.session.to_string(), session);
        assert_eq!(events.after, Some(7));
        assert_eq!(events.format, Format::Jsonl);
        assert_eq!(events.max_bytes, 128);
        assert!(events.follow);
        assert_eq!(
            events.cursor_file.as_deref(),
            Some(std::path::Path::new("/tmp/ctl-cursor"))
        );
    }

    // Defaults, and the global flags after the verb.
    let defaults = invocation(&["events", "--session", &session]);
    let Verb::Events(events) = &defaults.verb else {
        panic!("events did not parse as the events verb");
    };
    assert_eq!(events.format, Format::Text);
    assert_eq!(events.max_bytes, args::DEFAULT_MAX_BYTES);
    assert!(!events.follow);
    assert_eq!(events.after, None);

    let peek = invocation(&["peek", "--session", &session, "--lines=12"]);
    let Verb::Peek(peek) = &peek.verb else {
        panic!("peek did not parse as the peek verb");
    };
    assert_eq!(peek.lines, 12);
    assert_eq!(peek.max_bytes, args::DEFAULT_MAX_BYTES);

    let default_peek = invocation(&["peek", "--session", &session]);
    let Verb::Peek(peek) = &default_peek.verb else {
        panic!("peek did not parse as the peek verb");
    };
    assert_eq!(peek.lines, args::DEFAULT_LINES);

    let get = invocation(&["get", &session, "--metadata"]);
    assert!(matches!(get.verb, Verb::Get { metadata: true, .. }));

    let ls = invocation(&["ls"]);
    assert!(matches!(
        ls.verb,
        Verb::Ls {
            workspace: None,
            archived: false
        }
    ));
}

#[test]
fn the_parser_rejects_unknown_verbs_flags_and_missing_values() {
    assert!(parse_error(&["mystery"]).contains("unknown verb 'mystery'"));
    assert!(parse_error(&["--mystery", "whoami"]).contains("unknown flag '--mystery'"));
    assert!(parse_error(&["whoami", "--follow"]).contains("does not apply to 'whoami'"));
    assert!(parse_error(&["--connect"]).contains("--connect requires a value"));
    assert!(parse_error(&["--connect="]).contains("--connect requires a value"));
    assert!(parse_error(&["events"]).contains("requires --session"));
    assert!(parse_error(&["get"]).contains("requires a <session-id>"));
    assert!(
        parse_error(&["events", "--session", "not-an-id"]).contains("is not a valid session id")
    );
    assert!(
        parse_error(&[
            "events",
            "--session",
            &AgentSessionId::new().to_string(),
            "--max-bytes",
            "12"
        ])
        .contains("at least 64 bytes")
    );
    assert!(
        parse_error(&[
            "events",
            "--session",
            &AgentSessionId::new().to_string(),
            "--format",
            "yaml"
        ])
        .contains("must be text or jsonl")
    );
    assert!(parse_error(&[]).contains("missing verb"));
    assert!(parse_error(&["--max-frame-bytes", "0"]).contains("greater than zero"));
}

#[test]
fn the_worker_url_comes_from_the_flag_then_the_environment() {
    assert_eq!(
        args::resolve_connect(Some("ws://flag".to_owned()), Some("ws://env".to_owned())).as_deref(),
        Some("ws://flag")
    );
    assert_eq!(
        args::resolve_connect(None, Some("ws://env".to_owned())).as_deref(),
        Some("ws://env")
    );
    assert_eq!(args::resolve_connect(None, Some("   ".to_owned())), None);
    assert_eq!(args::resolve_connect(None, None), None);
}

#[test]
fn the_parser_applies_the_run_defaults_and_rejects_its_conflicts() {
    let workspace = WorkspaceId::new().to_string();
    let pinned = RequestId::new().to_string();
    let parsed = invocation(&[
        "run",
        "--workspace",
        &workspace,
        "--task",
        "  keep   this workspace tidy  ",
        "--request-id",
        &pinned,
    ]);
    let Verb::Run(args) = &parsed.verb else {
        panic!("run did not parse as the run verb");
    };
    assert_eq!(args.workspace.to_string(), workspace);
    assert_eq!(args.task, "  keep   this workspace tidy  ");
    assert_eq!(args.name, "keep this workspace tidy");
    assert_eq!(args.model.as_str(), "deterministic/demo");
    assert_eq!(args.approval, None);
    assert_eq!(
        args.request_id.map(|id| id.to_string()).as_deref(),
        Some(pinned.as_str())
    );
    assert!(args.repository.is_none());
    assert!(args.directory.is_none());

    // A name derived from the task keeps six words and at most 48 characters.
    assert_eq!(
        args::derived_name("one two three four five six seven eight"),
        "one two three four five six"
    );
    assert_eq!(args::derived_name("   \n\t"), "loom-cli run");
    assert_eq!(args::derived_name(&"a".repeat(100)).len(), 48);

    // An attach source lands at its final path component without `.git`.
    assert_eq!(
        args::default_attach_path("https://github.com/acme/widgets.git").as_deref(),
        Some("widgets")
    );
    assert_eq!(
        args::default_attach_path("/tmp/some/dir/").as_deref(),
        Some("dir")
    );
    assert_eq!(args::default_attach_path("/"), None);

    let repository = invocation(&[
        "run",
        "--workspace",
        &workspace,
        "--task",
        "clone",
        "--repo",
        "https://github.com/acme/widgets.git",
        "--revision",
        "main",
        "--path",
        "vendor",
        "--approval-policy",
        "auto",
    ]);
    let Verb::Run(args) = &repository.verb else {
        panic!("run did not parse as the run verb");
    };
    let attach = args.repository.as_ref().expect("a repository attach");
    assert_eq!(attach.source, "https://github.com/acme/widgets.git");
    assert_eq!(attach.path, "vendor");
    assert_eq!(attach.revision.as_deref(), Some("main"));
    assert_eq!(args.approval, Some(ApprovalPreset::Auto));

    let directory = invocation(&[
        "run",
        "--workspace",
        &workspace,
        "--task",
        "mount",
        "--as-directory=/tmp/demo",
    ]);
    let Verb::Run(args) = &directory.verb else {
        panic!("run did not parse as the run verb");
    };
    let attach = args.directory.as_ref().expect("a directory attach");
    assert_eq!(attach.source, "/tmp/demo");
    assert_eq!(attach.path, "demo");
    assert_eq!(attach.revision, None);

    assert!(parse_error(&["run", "--workspace", &workspace]).contains("requires --task"));
    assert!(
        parse_error(&[
            "run",
            "--workspace",
            &workspace,
            "--task",
            "a",
            "--task-file",
            "/tmp/t"
        ])
        .contains("not both")
    );
    assert!(
        parse_error(&[
            "run",
            "--workspace",
            &workspace,
            "--task",
            "a",
            "--repo",
            "/tmp/a",
            "--as-directory",
            "/tmp/b",
        ])
        .contains("not both")
    );
    assert!(
        parse_error(&[
            "run",
            "--workspace",
            &workspace,
            "--task",
            "a",
            "--as-directory",
            "/tmp/a",
            "--revision",
            "main",
        ])
        .contains("--revision only applies to --repo")
    );
    assert!(
        parse_error(&[
            "run",
            "--workspace",
            &workspace,
            "--task",
            "a",
            "--path",
            "x"
        ])
        .contains("--path requires")
    );
    assert!(
        parse_error(&[
            "run",
            "--workspace",
            &workspace,
            "--task",
            "a",
            "--approval-policy",
            "sometimes",
        ])
        .contains("must be default or auto")
    );
    assert!(
        parse_error(&[
            "run",
            "--workspace",
            &workspace,
            "--task",
            "a",
            "--request-id",
            "not-a-uuid",
        ])
        .contains("not a valid request id")
    );
    assert!(
        parse_error(&[
            "run",
            "--workspace",
            &workspace,
            "--task",
            "a",
            "--name",
            "  "
        ])
        .contains("--name must not be empty")
    );
    assert!(
        parse_error(&[
            "run",
            "--workspace",
            &workspace,
            "--task-file",
            "/tmp/loom-cli-definitely-absent-task-file",
        ])
        .contains("could not read the task file")
    );
    assert!(
        parse_error(&[
            "run",
            "--workspace",
            &workspace,
            "--task",
            "a",
            "--repo",
            "/",
        ])
        .contains("has no final path component")
    );
    assert!(parse_error(&["whoami", "--task", "a"]).contains("does not apply to 'whoami'"));
}

#[test]
fn help_short_circuits_parsing_whatever_else_is_given() {
    assert!(matches!(parse(&["--help"]), Ok(Parsed::Help)));
    assert!(matches!(parse(&["-h"]), Ok(Parsed::Help)));
    assert!(matches!(
        parse(&["events", "--help", "--mystery"]),
        Ok(Parsed::Help)
    ));
    assert!(args::HELP.contains("loom-cli"));
    assert!(args::HELP.contains("--cursor-file"));
    assert!(args::HELP.contains("--token-file"));
}

#[test]
fn result_reports_a_repository_and_states_when_there_is_nothing() {
    let fixture = fixture();
    let session_id = fixture.session.id;
    let session_text = session_id.to_string();
    let mut client = InProcessClient::new(&fixture.backend);

    // A session with no repository and no task has nothing to report.
    let empty = run(&mut client, &["result", "--session", &session_text]);
    assert_eq!(empty.code, exit::OK, "{}", empty.stderr);
    assert_eq!(
        empty.stdout,
        format!("session {session_id}\nnothing to report\n")
    );
    let envelope = json_envelope(&run(
        &mut client,
        &["result", "--session", &session_text, "--json"],
    ));
    assert_eq!(envelope["data"]["empty"], true);
    assert_eq!(envelope["data"]["repositories"], serde_json::json!([]));

    // A local repository with one commit and one modified file reports its
    // branch, head, diffstat, and changed path.
    let repository = temporary_path("result-repository");
    std::fs::create_dir_all(&repository).unwrap();
    if !git(&repository, &["init", "-q"]) {
        println!("skipping: git could not be spawned");
        std::fs::remove_dir_all(&repository).ok();
        return;
    }
    std::fs::write(repository.join("README"), "one\n").unwrap();
    assert!(git(&repository, &["add", "README"]));
    assert!(git(
        &repository,
        &[
            "-c",
            "user.name=loom cli test",
            "-c",
            "user.email=cli@example.invalid",
            "commit",
            "-q",
            "-m",
            "initial",
        ]
    ));
    let source = repository.to_string_lossy().into_owned();
    let path = repository
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let attached = fixture
        .control
        .request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::AttachSessionRepository {
                session_id,
                source,
                path: path.clone(),
                revision: None,
                reuse_local: false,
            },
        )));
    let repository_id = match attached.result {
        Ok(ServerResponse::Repository(RepositoryResponse::SessionRepositoryAttached(
            repository,
        ))) => repository.id,
        other => panic!("unexpected attach response: {other:?}"),
    };

    // The clone is clean; edit the session's copy so the result has a change
    // to report.
    let edited = fixture
        .control
        .request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::ApplySessionFilesystemEdit {
                session_id,
                edit: loom_core::WorkspaceEdit {
                    path: format!("{path}/README"),
                    old_text: "one\n".to_owned(),
                    new_text: "one\ntwo\n".to_owned(),
                    expected_revision: None,
                },
            },
        )));
    assert!(
        matches!(
            edited.result,
            Ok(ServerResponse::Filesystem(
                FilesystemResponse::WorkspaceEditApplied(_)
            ))
        ),
        "edit: {:?}",
        edited.result
    );

    let reported = run(
        &mut client,
        &["result", "--session", &session_text, "--json"],
    );
    assert_eq!(reported.code, exit::OK, "{}", reported.stderr);
    let envelope = json_envelope(&reported);
    assert_eq!(envelope["data"]["empty"], false);
    let reported_repository = &envelope["data"]["repositories"][0];
    assert_eq!(
        reported_repository["id"],
        repository_id.to_string().as_str()
    );
    assert_eq!(reported_repository["path"], path.as_str());
    assert!(
        reported_repository["branch"]
            .as_str()
            .is_some_and(|branch| !branch.is_empty()),
        "branch: {}",
        reported_repository["branch"]
    );
    assert_eq!(
        reported_repository["head"].as_str().map(str::len),
        Some(40),
        "head: {}",
        reported_repository["head"]
    );
    assert_eq!(reported_repository["changed_paths"][0]["path"], "README");
    assert_eq!(
        reported_repository["changed_paths"][0]["status"],
        "modified"
    );
    assert_eq!(reported_repository["diffstat"]["files"], 1);
    assert!(reported_repository["diffstat"]["added"].as_u64().unwrap() >= 1);

    let text = run(&mut client, &["result", "--session", &session_text]);
    assert_eq!(text.code, exit::OK, "{}", text.stderr);
    assert!(text.stdout.starts_with(&format!("session {session_id}\n")));
    assert!(
        text.stdout
            .contains(&format!("repository {repository_id} {path}\n")),
        "{}",
        text.stdout
    );
    assert!(text.stdout.contains("\ndiffstat 1 "), "{}", text.stdout);
    assert!(
        text.stdout.contains("\nchanged modified README\n"),
        "{}",
        text.stdout
    );

    // Task evidence is named when a task is asked for, and an empty set says so.
    let task = match fixture
        .control
        .request(RequestEnvelope::new(ClientRequest::Task(
            loom_protocol::TaskRequest::StartSessionTask {
                session_id,
                spec: loom_core::TaskSpec {
                    kind: loom_core::TaskKind::Command,
                    label: "noop".to_owned(),
                    command: "true".to_owned(),
                    args: Vec::new(),
                    cwd: None,
                    output_limit_bytes: None,
                    artifact_paths: Vec::new(),
                },
            },
        )))
        .result
    {
        Ok(ServerResponse::Task(loom_protocol::TaskResponse::TaskStarted(task))) => task.id,
        other => panic!("unexpected task response: {other:?}"),
    };
    let with_task = run(
        &mut client,
        &[
            "result",
            "--session",
            &session_text,
            "--task",
            &task.to_string(),
        ],
    );
    assert_eq!(with_task.code, exit::OK, "{}", with_task.stderr);
    assert!(
        with_task.stdout.contains("\nevidence none\n"),
        "{}",
        with_task.stdout
    );

    fixture.backend.shutdown().unwrap();
    std::fs::remove_dir_all(&repository).ok();
}

/// Runs one git command in `directory`, returning whether it succeeded.
fn git(directory: &std::path::Path, arguments: &[&str]) -> bool {
    match std::process::Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
    {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

#[test]
fn run_new_creates_a_fresh_session_and_replays_with_one_base() {
    let fixture = empty_fixture();
    let workspace = fixture.workspace.id.to_string();
    let mut client = InProcessClient::new(&fixture.backend);

    // A first run completes, leaving a session of that name in a terminal
    // state, which is exactly what a retrigger used to trip over.
    let first = run(
        &mut client,
        &[
            "run",
            "--workspace",
            &workspace,
            "--task",
            "retrigger",
            "--approval-policy",
            "auto",
            "--json",
        ],
    );
    assert_eq!(first.code, exit::OK, "{}", first.stderr);
    let first_envelope = json_envelope(&first);
    let first_session: AgentSessionId = first_envelope["data"]["session"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let waited = run(
        &mut client,
        &[
            "wait",
            "--session",
            &first_session.to_string(),
            "--until",
            "terminal",
            "--timeout",
            "10",
        ],
    );
    assert_eq!(waited.code, exit::OK, "{}", waited.stderr);

    // A plain run keeps reusing the session by name, and a completed session
    // refuses a second run.
    let reused = run(
        &mut client,
        &["run", "--workspace", &workspace, "--task", "retrigger"],
    );
    assert_eq!(reused.code, exit::GONE, "{}", reused.stderr);
    assert!(reused.stderr.contains("invalid_state"), "{}", reused.stderr);

    // `--new` creates a second session with the same name and starts it.
    let fresh = run(
        &mut client,
        &[
            "run",
            "--workspace",
            &workspace,
            "--task",
            "retrigger",
            "--new",
            "--json",
        ],
    );
    assert_eq!(fresh.code, exit::OK, "{}", fresh.stderr);
    let fresh_envelope = json_envelope(&fresh);
    assert_eq!(fresh_envelope["data"]["created"], true);
    let fresh_session: AgentSessionId = fresh_envelope["data"]["session"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_ne!(fresh_session, first_session, "a name is not an identity");
    let named = live_sessions(&fixture.control, fixture.workspace.id)
        .into_iter()
        .filter(|session| session.name == "retrigger")
        .count();
    assert_eq!(named, 2, "--new never matches a session by name");

    // One base drives the whole command: repeating it reuses the session and
    // the run instead of creating a second of either.
    let base = RequestId::new().to_string();
    let arguments = [
        "run",
        "--workspace",
        &workspace,
        "--task",
        "idempotent",
        "--new",
        "--request-id",
        &base,
        "--json",
    ];
    let once = run(&mut client, &arguments);
    assert_eq!(once.code, exit::OK, "{}", once.stderr);
    let twice = run(&mut client, &arguments);
    assert_eq!(twice.code, exit::OK, "{}", twice.stderr);
    let once = json_envelope(&once);
    let twice = json_envelope(&twice);
    assert_eq!(
        once["data"]["session"]["id"],
        twice["data"]["session"]["id"]
    );
    assert_eq!(once["data"]["run"]["id"], twice["data"]["run"]["id"]);
    assert_ne!(
        once["data"]["request_ids"]["session"], once["data"]["request_ids"]["run"],
        "the two mutations never share one id"
    );
    assert_eq!(once["data"]["request_ids"]["run"], base.as_str());
    let idempotent_session: AgentSessionId = once["data"]["session"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        live_sessions(&fixture.control, fixture.workspace.id)
            .into_iter()
            .filter(|session| session.name == "idempotent")
            .count(),
        1,
        "a replayed --new creates one session"
    );
    assert_eq!(
        started_run_ids(&fixture.control, idempotent_session).len(),
        1,
        "a replayed --new starts one run"
    );

    fixture.backend.shutdown().unwrap();
}

#[test]
fn the_parser_covers_the_control_verbs_and_rejects_their_mistakes() {
    let workspace = WorkspaceId::new().to_string();
    let session = AgentSessionId::new().to_string();
    let run = RunId::new().to_string();
    let call = ToolCallId::new().to_string();
    let task = loom_core::TaskId::new().to_string();

    assert!(matches!(
        invocation(&["send", "--run", &run, "--text", "go"]).verb,
        Verb::Send { .. }
    ));
    assert!(matches!(
        invocation(&["answer", "--run", &run, "--text", "yes"]).verb,
        Verb::Answer { .. }
    ));
    assert!(matches!(
        invocation(&["approve", "--run", &run, "--tool-call", &call]).verb,
        Verb::Approve { .. }
    ));
    assert!(matches!(
        invocation(&[
            "deny",
            "--run",
            &run,
            "--tool-call",
            &call,
            "--reason",
            "no"
        ])
        .verb,
        Verb::Deny { .. }
    ));
    assert!(matches!(
        invocation(&["wait", "--session", &session, "--until", "terminal"]).verb,
        Verb::Wait {
            until: args::WaitUntil::Terminal,
            ..
        }
    ));
    assert!(matches!(
        invocation(&["status", "--session", &session, "--format", "verb"]).verb,
        Verb::Status {
            format: args::StatusFormat::Verb,
            ..
        }
    ));
    assert!(matches!(
        invocation(&["liveness", "--session", &session]).verb,
        Verb::Liveness { .. }
    ));
    assert!(matches!(
        invocation(&["result", "--session", &session, "--task", &task]).verb,
        Verb::Result { .. }
    ));
    let Verb::Run(run_args) =
        invocation(&["run", "--workspace", &workspace, "--task", "a", "--new"]).verb
    else {
        panic!("run --new parses as run");
    };
    assert!(run_args.new);

    assert!(parse_error(&["send", "--text", "go"]).contains("requires --run"));
    assert!(parse_error(&["answer", "--run", &run]).contains("requires --text"));
    assert!(parse_error(&["approve", "--run", &run]).contains("requires --tool-call"));
    assert!(parse_error(&["stop", "--run", &run, "--archive"]).contains("requires --session"));
    assert!(parse_error(&["stop", "--run", &run, "--session", &session]).contains("only applies"));
    assert!(
        parse_error(&["wait", "--session", &session, "--until", "soon"])
            .contains("blocked or terminal")
    );
    assert!(
        parse_error(&["wait", "--session", &session, "--timeout", "0"])
            .contains("greater than zero")
    );
    assert!(
        parse_error(&["status", "--session", &session, "--format", "jsonl"])
            .contains("text or verb")
    );
    assert!(parse_error(&["liveness"]).contains("requires --session"));
    assert!(
        parse_error(&["result", "--session", &session, "--task", "no"])
            .contains("not a valid task id")
    );
}

#[test]
fn wait_reports_blocked_and_terminal_and_is_always_bounded() {
    // Blocked: the run parks on the first approval and the wait returns at once
    // with the attempt and the tool call in both the text and the envelope.
    let blocked_fixture = fixture();
    let session_id = blocked_fixture.session.id;
    pin_default_policy(&blocked_fixture.control, session_id);
    let run_id = start_run(&blocked_fixture.control, session_id);
    let projection = await_run_state(
        &blocked_fixture.control,
        session_id,
        AgentRunState::AwaitingApproval,
    );
    let attempt = projection
        .active_run
        .as_ref()
        .expect("a run")
        .run
        .attempt_id;
    let call = projection
        .active_run
        .as_ref()
        .and_then(|active| active.pending_approval.as_ref())
        .map(|call| call.id)
        .expect("a pending approval");
    let session_text = session_id.to_string();
    let mut client = InProcessClient::new(&blocked_fixture.backend);

    let blocked = run(
        &mut client,
        &[
            "wait",
            "--session",
            &session_text,
            "--until",
            "blocked",
            "--timeout",
            "10",
        ],
    );
    assert_eq!(blocked.code, exit::BLOCKED, "{}", blocked.stderr);
    assert_eq!(
        blocked.stdout,
        format!("run {run_id} awaiting_approval\nblocked {attempt} {call}\n")
    );

    let blocked_json = run(
        &mut client,
        &[
            "wait",
            "--session",
            &session_text,
            "--timeout",
            "10",
            "--json",
        ],
    );
    assert_eq!(blocked_json.code, exit::BLOCKED, "{}", blocked_json.stderr);
    let envelope = json_envelope(&blocked_json);
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["blocked"], true);
    assert_eq!(envelope["data"]["attempt_id"], attempt.to_string().as_str());
    assert_eq!(envelope["data"]["tool_call_id"], call.to_string().as_str());

    // Terminal: an auto-approved run completes, and the wait exits 0.
    let completed_fixture = fixture();
    let completed_session = completed_fixture.session.id;
    pin_auto_policy(&completed_fixture.control, completed_session);
    let completed_run = start_run(&completed_fixture.control, completed_session);
    let mut completed_client = InProcessClient::new(&completed_fixture.backend);
    let completed = run(
        &mut completed_client,
        &[
            "wait",
            "--session",
            &completed_session.to_string(),
            "--until",
            "terminal",
            "--timeout",
            "10",
        ],
    );
    assert_eq!(completed.code, exit::OK, "{}", completed.stderr);
    assert_eq!(completed.stdout, format!("run {completed_run} completed\n"));

    // A cancelled run is terminal and reports a documented non-zero code.
    let stop_fixture = fixture();
    let stop_session = stop_fixture.session.id;
    pin_default_policy(&stop_fixture.control, stop_session);
    let stop_run = start_run(&stop_fixture.control, stop_session);
    await_run_state(
        &stop_fixture.control,
        stop_session,
        AgentRunState::AwaitingApproval,
    );
    let stop_text = stop_run.to_string();
    let mut stop_client = InProcessClient::new(&stop_fixture.backend);
    assert_eq!(
        run(&mut stop_client, &["stop", "--run", &stop_text]).code,
        exit::OK
    );
    let cancelled = run(
        &mut stop_client,
        &[
            "wait",
            "--session",
            &stop_session.to_string(),
            "--until",
            "terminal",
            "--timeout",
            "10",
        ],
    );
    assert_eq!(cancelled.code, exit::INTERNAL, "{}", cancelled.stderr);
    assert_eq!(cancelled.stdout, format!("run {stop_run} cancelled\n"));

    // A session with no run has nothing to observe, so the bound reports a
    // timeout instead of waiting forever.
    let idle_fixture = fixture();
    let mut idle_client = InProcessClient::new(&idle_fixture.backend);
    let idle = run(
        &mut idle_client,
        &[
            "wait",
            "--session",
            &idle_fixture.session.id.to_string(),
            "--until",
            "terminal",
            "--timeout",
            "1",
        ],
    );
    assert_eq!(idle.code, exit::TIMEOUT, "{}", idle.stderr);
    assert!(idle.stderr.contains("deadline_exceeded"), "{}", idle.stderr);
}

#[test]
fn status_prints_its_lines_and_the_derived_declaration() {
    let fixture = fixture();
    let session_id = fixture.session.id;
    let session_text = session_id.to_string();
    let mut client = InProcessClient::new(&fixture.backend);

    let idle = run(&mut client, &["status", "--session", &session_text]);
    assert_eq!(idle.code, exit::OK, "{}", idle.stderr);
    assert_eq!(
        idle.stdout,
        format!("session {session_id} idle {}\n", fixture.session.name)
    );
    let idle_verb = run(
        &mut client,
        &["status", "--session", &session_text, "--format", "verb"],
    );
    assert_eq!(idle_verb.stdout, "done: idle -\n");

    pin_default_policy(&fixture.control, session_id);
    let run_id = start_run(&fixture.control, session_id);
    let projection = await_run_state(
        &fixture.control,
        session_id,
        AgentRunState::AwaitingApproval,
    );
    let call = projection
        .active_run
        .as_ref()
        .and_then(|active| active.pending_approval.as_ref())
        .map(|call| call.id)
        .expect("a pending approval");
    let blocked = run(&mut client, &["status", "--session", &session_text]);
    assert_eq!(blocked.code, exit::OK, "{}", blocked.stderr);
    assert_eq!(
        blocked.stdout,
        format!(
            "session {session_id} awaiting_approval {}\nrun {run_id} awaiting_approval\npending {call}\n",
            fixture.session.name
        )
    );
    let verb = run(
        &mut client,
        &[
            "status",
            "--session",
            &session_text,
            "--format",
            "verb",
            "--json",
        ],
    );
    assert_eq!(verb.code, exit::OK, "{}", verb.stderr);
    let envelope = json_envelope(&verb);
    assert_eq!(envelope["data"]["verdict"], "blocked");
    assert_eq!(
        envelope["data"]["declaration"],
        format!("blocked: awaiting_approval {run_id}")
    );
    assert_eq!(envelope["data"]["state"], "awaiting_approval");
}

#[test]
fn liveness_reports_each_verdict_with_its_exit_code() {
    let fixture = fixture();
    let session_text = fixture.session.id.to_string();
    let mut client = InProcessClient::new(&fixture.backend);

    let alive = run(&mut client, &["liveness", "--session", &session_text]);
    assert_eq!(alive.code, exit::OK, "{}", alive.stderr);
    assert_eq!(
        alive.stdout,
        format!("liveness alive {}\n", fixture.session.id)
    );

    // An archived session is dead, not gone: the worker knows it.
    let archived = fixture
        .control
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::ArchiveAgentSession {
                session_id: fixture.session.id,
            },
        )));
    assert!(matches!(
        archived.result,
        Ok(ServerResponse::Session(
            SessionResponse::AgentSessionArchived(_)
        ))
    ));
    let dead = run(&mut client, &["liveness", "--session", &session_text]);
    assert_eq!(dead.code, exit::OK, "{}", dead.stderr);
    assert_eq!(
        dead.stdout,
        format!("liveness dead {}\n", fixture.session.id)
    );

    // An id the worker does not know is gone.
    let unknown_id = AgentSessionId::new();
    let gone = run(
        &mut client,
        &["liveness", "--session", &unknown_id.to_string()],
    );
    assert_eq!(gone.code, exit::GONE, "{}", gone.stderr);
    assert_eq!(gone.stdout, format!("liveness gone {unknown_id}\n"));

    // A transport failure is unknown, never gone.
    let mut unreachable = Wrapped::new(InProcessClient::new(&fixture.backend)).fail_on(|request| {
        matches!(
            request,
            ClientRequest::Session(SessionRequest::GetAgentSessionSnapshotMetadata { .. })
        )
        .then(|| {
            loom_core::LoomError::new(
                loom_core::ErrorCode::Internal,
                "the connection dropped",
                true,
            )
        })
    });
    let unknown = run(
        &mut unreachable,
        &["liveness", "--session", &session_text, "--json"],
    );
    assert_eq!(unknown.code, exit::UNREACHABLE, "{}", unknown.stderr);
    let envelope = json_envelope(&unknown);
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["verdict"], "unknown");
    assert_eq!(envelope["data"]["state"], serde_json::Value::Null);
}
