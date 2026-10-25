//! Tests for the `loom-cli` contract, driven through the in-process backend.
//!
//! Every test talks to a real `InProcessBackend` over the same `Client` seam the
//! binary uses, so the pinned output, exit codes, and cursor behaviour are the
//! behaviour of the shipped code rather than of a test double. The only test
//! doubles rewrite a protocol answer that would otherwise need a pruned journal
//! or a second worker.

use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::Arc, time::Duration};

use loom_core::{
    AgentSessionId, AgentSessionSnapshot, AgentSessionState, ApprovalPolicy, Author, Capability,
    CapabilitySet, CollaborationEntryKind, DelegatedTaskRecord, DelegatedTaskStatus, EventSequence,
    GitDiff, GitDiffHunk, GitDiffLine, GitDiffLineKind, GitFileStatus, GitFileStatusKind,
    GitRepositoryStatus, ProjectAgentPermissions, ProjectAgentRecord, ProjectId, ProjectSnapshot,
    ProjectWorktreeCleanupDisposition, ProjectWorktreeRecord, ProjectWorktreeStatus,
    ProtocolVersion, RepositoryId, RequestId, RunId, TaskId, Timestamp, ToolCallId, WorkspaceId,
};
use loom_model::ModelId;
use loom_protocol::{
    ActivityDetail, AgentEvent, AgentRunSnapshot, AgentRunState, AgentSessionSnapshotProjection,
    CURRENT_PROTOCOL_VERSION, ClientRequest, CollaborationRequest, ControlRequest, ControlResponse,
    EventsRequest, EventsResponse, FilesystemRequest, FilesystemResponse, ProjectRequest,
    ProjectResponse, RepositoryRequest, RepositoryResponse, RequestEnvelope, ResponseEnvelope,
    RunRequest, RunResponse, ServerEvent, ServerEventEnvelope, ServerResponse, SessionDirectory,
    SessionRepository, SessionRequest, SessionResponse, WorkspaceRequest, WorkspaceResponse,
};
use loom_server::{InProcessBackend, InProcessConnection};

use crate::cli::args::{self, ApprovalPreset, Format, Parsed, Verb, WorktreeDisposition};
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

/// A durable fixture, needed by the verbs the backend only offers when it has
/// state to persist: archive-retention changes and session deletion.
fn persistent_fixture(label: &str) -> (Fixture, PathBuf) {
    let directory = temporary_path(label);
    std::fs::create_dir_all(&directory).unwrap();
    let backend = InProcessBackend::new_persistent(directory.join("state.sqlite")).unwrap();
    let control = backend.connect();
    negotiate(&control);
    let workspace = create_workspace(&control, "persistent test workspace");
    let session = create_session(&control, workspace.id, "persistent test session");
    (
        Fixture {
            backend,
            control,
            workspace,
            session,
        },
        directory,
    )
}

fn close_persistent_fixture(fixture: Fixture, directory: PathBuf) {
    let Fixture {
        backend, control, ..
    } = fixture;
    drop(control);
    let _ = backend.shutdown();
    drop(backend);
    let _ = std::fs::remove_dir_all(directory);
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
            vec!["node".to_owned()],
            vec!["retention".to_owned(), "get".to_owned()],
        ] {
            let outcome = run_async_owned(&mut client, &arguments).await;
            assert_eq!(outcome.code, exit::OK, "{arguments:?}: {}", outcome.stderr);
        }

        // The read-only token carries neither operator capability, so both
        // writes are refused with exit 3 and change nothing.
        for arguments in [
            vec![
                "retention".to_owned(),
                "set".to_owned(),
                "--window".to_owned(),
                "14d".to_owned(),
            ],
            vec!["delete".to_owned(), session_text.clone()],
        ] {
            let outcome = run_async_owned(&mut client, &arguments).await;
            assert_eq!(
                outcome.code,
                exit::BLOCKED,
                "{arguments:?}: {}",
                outcome.stderr
            );
            assert_eq!(outcome.stdout, "", "{arguments:?}");
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
    let task = TaskId::new().to_string();
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
            vec!["status", "--session", &session, "--tree"],
            vec![
                Capability::JsonProtocol,
                Capability::ReadAgentSession,
                Capability::ReadProject,
            ],
        ),
        (
            vec!["liveness", "--session", &session],
            vec![Capability::JsonProtocol, Capability::ReadAgentSession],
        ),
        (
            vec!["node"],
            vec![Capability::JsonProtocol, Capability::ReadWorkerNodeStatus],
        ),
        (
            vec!["retention", "get"],
            vec![Capability::JsonProtocol, Capability::ReadWorkerNodeStatus],
        ),
        (
            vec!["retention", "set", "--window", "14d"],
            vec![
                Capability::JsonProtocol,
                Capability::ReadWorkerNodeStatus,
                Capability::ConfigureArchiveRetention,
            ],
        ),
        (
            vec!["delete", &session],
            vec![
                Capability::JsonProtocol,
                Capability::ReadProject,
                Capability::DeleteAgentSession,
            ],
        ),
        (
            vec!["session", "archive", &session],
            vec![
                Capability::JsonProtocol,
                Capability::ReadProject,
                Capability::ControlAgentSession,
            ],
        ),
        (
            vec!["session", "delete", &session],
            vec![
                Capability::JsonProtocol,
                Capability::ReadProject,
                Capability::DeleteAgentSession,
            ],
        ),
        (
            vec!["project", "archive", &session],
            vec![
                Capability::JsonProtocol,
                Capability::ReadProject,
                Capability::ControlAgentSession,
            ],
        ),
        (
            vec!["project", "delete", &session],
            vec![
                Capability::JsonProtocol,
                Capability::ReadProject,
                Capability::DeleteAgentSession,
            ],
        ),
        (
            vec![
                "project", "worktree", "cleanup", &session, "--task", &task, "--keep",
            ],
            vec![
                Capability::JsonProtocol,
                Capability::ReadProject,
                Capability::CleanupProjectChildWorktree,
            ],
        ),
        (
            vec!["project", "worktrees", &session],
            vec![Capability::JsonProtocol, Capability::ReadProject],
        ),
        (
            vec!["project", "worktree", "get", &session, "--task", &task],
            vec![Capability::JsonProtocol, Capability::ReadProject],
        ),
        (
            vec!["project", "worktree", "diff", &session, "--task", &task],
            vec![
                Capability::JsonProtocol,
                Capability::ReadProject,
                Capability::ReadProjectChildReview,
            ],
        ),
        (
            vec![
                "project",
                "worktree",
                "integrate",
                &session,
                "--task",
                &task,
            ],
            vec![
                Capability::JsonProtocol,
                Capability::ReadProject,
                Capability::IntegrateProjectChild,
            ],
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
/// projection that carries it. A run that is awaiting approval also has to
/// carry its pending tool call: the state and the call are written separately,
/// so a projection can report the state one read before it carries the call.
fn await_run_state(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
    wanted: AgentRunState,
) -> AgentSessionSnapshotProjection {
    for _ in 0..1200 {
        let projection = session_projection(connection, session_id);
        if let Some(active) = &projection.active_run
            && active.run.state == wanted
            && (wanted != AgentRunState::AwaitingApproval || active.pending_approval.is_some())
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
        invocation(&["status", "--session", &session]).verb,
        Verb::Status { tree: false, .. }
    ));
    assert!(matches!(
        invocation(&["status", "--session", &session, "--tree"]).verb,
        Verb::Status { tree: true, .. }
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
    assert!(
        parse_error(&[
            "status",
            "--session",
            &session,
            "--tree",
            "--format",
            "verb"
        ])
        .contains("--format text")
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
fn status_tree_reports_a_standalone_session_as_a_one_agent_project() {
    let fixture = fixture();
    let session_id = fixture.session.id;
    let session_text = session_id.to_string();
    let mut client = InProcessClient::new(&fixture.backend);

    // A session that no project records is a one-agent project of its own, and
    // its project id is its own id.
    let tree = run(
        &mut client,
        &["status", "--session", &session_text, "--tree"],
    );
    assert_eq!(tree.code, exit::OK, "{}", tree.stderr);
    assert_eq!(
        tree.stdout,
        format!(
            "session {session_id} idle {}\nproject {session_id} done\nroot {session_id}\nagent {session_id} 1 idle done -\n",
            fixture.session.name
        )
    );

    let json = run(
        &mut client,
        &["status", "--session", &session_text, "--tree", "--json"],
    );
    assert_eq!(json.code, exit::OK, "{}", json.stderr);
    let envelope = json_envelope(&json);
    let project = &envelope["data"]["project"];
    assert_eq!(project["id"], session_text.as_str());
    assert_eq!(project["root_session_id"], session_text.as_str());
    assert_eq!(project["parent_project_id"], serde_json::Value::Null);
    assert_eq!(project["verdict"], "done");
    assert_eq!(
        project["counts"],
        serde_json::json!({"agents": 1, "tasks": 0, "worktrees": 0, "decisions": 0})
    );
    assert_eq!(project["agents"][0]["session_id"], session_text.as_str());
    assert_eq!(project["agents"][0]["depth"], 1);
    assert_eq!(project["agents"][0]["state"], "idle");
    assert_eq!(project["agents"][0]["verdict"], "done");
    assert_eq!(
        project["agents"][0]["task_summary"],
        serde_json::Value::Null
    );
    assert!(project["tasks"].as_array().unwrap().is_empty());
    assert!(project["worktrees"].as_array().unwrap().is_empty());
    assert!(project["decisions"].as_array().unwrap().is_empty());
    // The keys of the plain payload stay as they are.
    assert_eq!(envelope["data"]["state"], "idle");
    assert_eq!(envelope["data"]["verdict"], "done");
    assert_eq!(
        envelope["data"]["pending_tool_call"],
        serde_json::Value::Null
    );
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

// 9. Operator verbs: worker node status, archive retention, and deletion.

fn scripted_node() -> loom_protocol::WorkerNodeStatus {
    loom_protocol::WorkerNodeStatus {
        node_id: "node-1".to_owned(),
        name: "worker".to_owned(),
        online: true,
        capabilities: CapabilitySet::new([
            Capability::ReadWorkerNodeStatus,
            Capability::JsonProtocol,
        ]),
        resources: loom_protocol::WorkerNodeResources {
            cpu_count: 8,
            cpu_usage_percent: Some(42),
            memory_usage_percent: None,
            memory_total_bytes: Some(17_179_869_184),
            memory_available_bytes: Some(8_589_934_592),
            disk_total_bytes: Some(512_110_190_592),
            disk_available_bytes: None,
        },
    }
}

#[test]
fn node_prints_one_line_per_field_and_the_json_envelope() {
    let fixture = fixture();
    let node = scripted_node();
    let scripted = node.clone();
    let mut client =
        Wrapped::new(InProcessClient::new(&fixture.backend)).after(move |request, response| {
            if matches!(
                request,
                ClientRequest::Control(ControlRequest::GetWorkerNodeStatus)
            ) {
                response.result = Ok(ServerResponse::Control(ControlResponse::WorkerNodeStatus(
                    scripted.clone(),
                )));
            }
        });

    let text = run(&mut client, &["node"]);
    assert_eq!(text.code, exit::OK, "{}", text.stderr);
    assert_eq!(text.stderr, "");
    assert_eq!(
        text.stdout,
        "node_id node-1\n\
         name worker\n\
         online true\n\
         capabilities 2 read_worker_node_status,json_protocol\n\
         cpu_count 8\n\
         cpu_usage_percent 42\n\
         memory_usage_percent unknown\n\
         memory_total_bytes 17179869184\n\
         memory_available_bytes 8589934592\n\
         disk_total_bytes 512110190592\n\
         disk_available_bytes unknown\n"
    );

    let envelope = run(&mut client, &["node", "--json"]);
    assert_eq!(envelope.code, exit::OK, "{}", envelope.stderr);
    let value = json_envelope(&envelope);
    assert_eq!(value["verb"], "node");
    assert_eq!(value["data"], serde_json::to_value(&node).unwrap());
}

#[test]
fn retention_get_and_set_report_and_persist_the_worker_policy() {
    let directory = temporary_path("retention");
    std::fs::create_dir_all(&directory).unwrap();
    let database = directory.join("state.sqlite");

    {
        let backend = InProcessBackend::new_persistent(&database).unwrap();
        let mut client = InProcessClient::new(&backend);

        let default = run(&mut client, &["retention", "get"]);
        assert_eq!(default.code, exit::OK, "{}", default.stderr);
        assert_eq!(
            default.stdout,
            "window off\nforce-discard false\nsub-agent 10m\n"
        );

        let set = run(
            &mut client,
            &[
                "retention",
                "set",
                "--window",
                "14d",
                "--force-discard",
                "true",
                "--sub-agent",
                "10m",
            ],
        );
        assert_eq!(set.code, exit::OK, "{}", set.stderr);
        assert_eq!(
            set.stdout,
            "window 14d\nforce-discard true\nsub-agent 10m\n\
             stored for this worker; the next sweep applies it\n"
        );

        let read_back = run(&mut client, &["retention", "get", "--json"]);
        assert_eq!(read_back.code, exit::OK, "{}", read_back.stderr);
        let envelope = json_envelope(&read_back);
        assert_eq!(envelope["verb"], "retention get");
        assert_eq!(envelope["data"]["window"], "14d");
        assert_eq!(envelope["data"]["force_discard_worktrees"], true);
        assert_eq!(envelope["data"]["sub_agent"], "10m");
        backend.shutdown().unwrap();
    }

    // A restart on the same state database keeps the stored policy without a
    // flag, exactly as the server flags promise.
    {
        let reopened = InProcessBackend::new_persistent(&database).unwrap();
        let mut client = InProcessClient::new(&reopened);
        let persisted = run(&mut client, &["retention", "get"]);
        assert_eq!(persisted.code, exit::OK, "{}", persisted.stderr);
        assert_eq!(
            persisted.stdout,
            "window 14d\nforce-discard true\nsub-agent 10m\n"
        );

        let set_json = run(
            &mut client,
            &["retention", "set", "--window", "off", "--json"],
        );
        assert_eq!(set_json.code, exit::OK, "{}", set_json.stderr);
        let envelope = json_envelope(&set_json);
        assert_eq!(envelope["verb"], "retention set");
        assert_eq!(envelope["data"]["window"], "off");
        assert_eq!(envelope["data"]["force_discard_worktrees"], true);
        assert_eq!(envelope["data"]["applies_from"], "next_sweep");

        let off = run(&mut client, &["retention", "get"]);
        assert_eq!(
            off.stdout,
            "window off\nforce-discard true\nsub-agent 10m\n"
        );
        reopened.shutdown().unwrap();
    }

    // The disabled window is durable too, and the other fields keep their
    // stored values because the update only named the window.
    {
        let reopened = InProcessBackend::new_persistent(&database).unwrap();
        let mut client = InProcessClient::new(&reopened);
        let persisted = run(&mut client, &["retention", "get"]);
        assert_eq!(persisted.code, exit::OK, "{}", persisted.stderr);
        assert_eq!(
            persisted.stdout,
            "window off\nforce-discard true\nsub-agent 10m\n"
        );
        reopened.shutdown().unwrap();
    }
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn retention_set_rejects_a_bad_duration_and_an_empty_update() {
    let fixture = fixture();
    let mut client = InProcessClient::new(&fixture.backend);

    let bad = run(&mut client, &["retention", "set", "--window", "later"]);
    assert_eq!(bad.code, exit::USAGE);
    assert_eq!(bad.stdout, "");
    for expected in ["--window", "30s", "14d", "off", "later"] {
        assert!(bad.stderr.contains(expected), "{}", bad.stderr);
    }

    let bad_sub_agent = run(&mut client, &["retention", "set", "--sub-agent", "2x"]);
    assert_eq!(bad_sub_agent.code, exit::USAGE);
    assert!(bad_sub_agent.stderr.contains("--sub-agent"));

    let bad_force = run(
        &mut client,
        &["retention", "set", "--force-discard", "sometimes"],
    );
    assert_eq!(bad_force.code, exit::USAGE);
    assert!(bad_force.stderr.contains("--force-discard"));

    let empty = run(&mut client, &["retention", "set"]);
    assert_eq!(empty.code, exit::USAGE);
    assert!(
        empty.stderr.contains("requires --window"),
        "{}",
        empty.stderr
    );

    let unknown = run(&mut client, &["retention", "show"]);
    assert_eq!(unknown.code, exit::USAGE);
    assert!(unknown.stderr.contains("get or set"), "{}", unknown.stderr);

    let no_sub_verb = run(&mut client, &["retention"]);
    assert_eq!(no_sub_verb.code, exit::USAGE);
    assert!(
        no_sub_verb.stderr.contains("requires 'get' or 'set'"),
        "{}",
        no_sub_verb.stderr
    );
}

fn archive_session(connection: &InProcessConnection, session_id: AgentSessionId) {
    let archived = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::ArchiveAgentSession { session_id },
    )));
    assert!(matches!(
        archived.result,
        Ok(ServerResponse::Session(
            SessionResponse::AgentSessionArchived(_)
        ))
    ));
}

fn fake_project(
    root: AgentSessionId,
    agents: &[AgentSessionId],
    worktrees: Vec<ProjectWorktreeRecord>,
) -> ProjectSnapshot {
    ProjectSnapshot {
        project_id: ProjectId::from_uuid(*root.as_uuid()),
        root_session_id: root,
        parent_project_id: None,
        agents: agents
            .iter()
            .map(|session_id| ProjectAgentRecord {
                session_id: *session_id,
                project_id: ProjectId::from_uuid(*root.as_uuid()),
                parent_session_id: (*session_id != root).then_some(root),
                depth: if *session_id == root { 1 } else { 2 },
                state: AgentSessionState::Archived,
                task_summary: None,
                output_cursor: EventSequence::default(),
                updated_at: Timestamp::now(),
            })
            .collect(),
        tasks: Vec::new(),
        worktrees,
    }
}

fn fake_task(
    project_id: ProjectId,
    requester: AgentSessionId,
    target: AgentSessionId,
    name: &str,
) -> DelegatedTaskRecord {
    DelegatedTaskRecord {
        task_id: TaskId::new(),
        project_id,
        requester_session_id: requester,
        target_session_id: target,
        child_name: name.to_owned(),
        intent: "review the change".to_owned(),
        model_id: "deterministic/demo".to_owned(),
        context_references: Vec::new(),
        dependencies: Vec::new(),
        code_change: true,
        permissions: ProjectAgentPermissions::default(),
        status: DelegatedTaskStatus::Running,
        created_at: Timestamp::now(),
        updated_at: Timestamp::now(),
    }
}

fn fake_worktree(
    parent: AgentSessionId,
    child: AgentSessionId,
    name: &str,
    path: &str,
) -> ProjectWorktreeRecord {
    ProjectWorktreeRecord {
        project_id: ProjectId::from_uuid(*parent.as_uuid()),
        task_id: TaskId::new(),
        parent_session_id: parent,
        child_session_id: child,
        parent_repository_id: RepositoryId::new(),
        child_repository_id: RepositoryId::new(),
        relative_path: path.to_owned(),
        worktree_name: name.to_owned(),
        branch_name: "loom/test".to_owned(),
        base_revision: "0".repeat(40),
        base_refresh: None,
        result_revision: None,
        integrated_revision: None,
        status: ProjectWorktreeStatus::Ready,
        conflict_paths: Vec::new(),
        error: None,
        cleanup_disposition: None,
        created_at: Timestamp::now(),
        updated_at: Timestamp::now(),
    }
}

fn with_project_snapshot(
    client: Wrapped<InProcessClient>,
    target: AgentSessionId,
    project: ProjectSnapshot,
) -> Wrapped<InProcessClient> {
    client.after(move |request, response| {
        if matches!(
            request,
            ClientRequest::Project(ProjectRequest::GetProjectSnapshotForSession { session_id })
                if *session_id == target
        ) {
            response.result = Ok(ServerResponse::Project(ProjectResponse::ProjectSnapshot(
                project.clone(),
            )));
        }
    })
}

#[test]
fn status_tree_projects_agents_tasks_worktrees_and_decisions() {
    let fixture = fixture();
    let root = fixture.session.id;
    let child = create_session(&fixture.control, fixture.workspace.id, "project child").id;
    pin_default_policy(&fixture.control, child);
    let run_id = start_run(&fixture.control, child);
    let projection = await_run_state(&fixture.control, child, AgentRunState::AwaitingApproval);
    let approval = projection
        .active_run
        .as_ref()
        .and_then(|active| active.pending_approval.as_ref())
        .expect("a pending approval");
    let (call, tool_name) = (approval.id, approval.name.clone());

    let mut project = fake_project(root, &[root, child], Vec::new());
    project.parent_project_id = Some(ProjectId::new());
    project.agents[0].state = AgentSessionState::Idle;
    project.agents[1].state = AgentSessionState::AwaitingApproval;
    project.agents[1].task_summary = Some("wire the status tree".to_owned());
    project.tasks = vec![fake_task(project.project_id, root, child, "reviewer")];
    let task_id = project.tasks[0].task_id;
    let mut worktree = fake_worktree(root, child, "wt-1", "project-worktrees/child");
    worktree.status = ProjectWorktreeStatus::Conflict;
    worktree.result_revision = Some("1".repeat(40));
    worktree.integrated_revision = Some("2".repeat(40));
    worktree.conflict_paths = vec!["src/main.rs".to_owned()];
    project.worktrees = vec![worktree];
    let project_id = project.project_id;
    let parent_project_id = project.parent_project_id.expect("a parent project");
    let (base_revision, result_revision, integrated_revision) = (
        project.worktrees[0].base_revision.clone(),
        "1".repeat(40),
        "2".repeat(40),
    );

    let mut client = with_project_snapshot(
        Wrapped::new(InProcessClient::new(&fixture.backend)),
        child,
        project,
    );
    let child_text = child.to_string();
    let tree = run(&mut client, &["status", "--session", &child_text, "--tree"]);
    assert_eq!(tree.code, exit::OK, "{}", tree.stderr);
    let expected = [
        format!("session {child} awaiting_approval project child"),
        format!("run {run_id} awaiting_approval"),
        format!("project {project_id} blocked"),
        format!("parent {parent_project_id}"),
        format!("root {root}"),
        format!("agent {root} 1 idle done -"),
        format!("agent {child} 2 awaiting_approval blocked wire the status tree"),
        format!("task {task_id} running reviewer"),
        format!(
            "worktree wt-1 conflict project-worktrees/child {base_revision} {result_revision} \
             {integrated_revision}"
        ),
        "conflict wt-1 src/main.rs".to_owned(),
        format!("decision approval {child} {call} {tool_name}"),
    ]
    .join("\n");
    assert_eq!(tree.stdout, format!("{expected}\n"));

    let json = run(
        &mut client,
        &["status", "--session", &child_text, "--tree", "--json"],
    );
    assert_eq!(json.code, exit::OK, "{}", json.stderr);
    let envelope = json_envelope(&json);
    let project = &envelope["data"]["project"];
    assert_eq!(project["id"], project_id.to_string().as_str());
    assert_eq!(project["root_session_id"], root.to_string().as_str());
    assert_eq!(
        project["parent_project_id"],
        parent_project_id.to_string().as_str()
    );
    assert_eq!(project["verdict"], "blocked");
    assert_eq!(
        project["counts"],
        serde_json::json!({"agents": 2, "tasks": 1, "worktrees": 1, "decisions": 1})
    );
    assert_eq!(project["agents"][0]["verdict"], "done");
    assert_eq!(project["agents"][1]["verdict"], "blocked");
    assert_eq!(project["agents"][1]["state"], "awaiting_approval");
    assert_eq!(project["agents"][1]["task_summary"], "wire the status tree");
    assert_eq!(project["tasks"][0]["task_id"], task_id.to_string().as_str());
    assert_eq!(project["tasks"][0]["status"], "running");
    assert_eq!(project["tasks"][0]["child_name"], "reviewer");
    assert_eq!(project["worktrees"][0]["worktree_name"], "wt-1");
    assert_eq!(project["worktrees"][0]["status"], "conflict");
    assert_eq!(
        project["worktrees"][0]["relative_path"],
        "project-worktrees/child"
    );
    assert_eq!(
        project["worktrees"][0]["base_revision"],
        base_revision.as_str()
    );
    assert_eq!(
        project["worktrees"][0]["result_revision"],
        result_revision.as_str()
    );
    assert_eq!(
        project["worktrees"][0]["integrated_revision"],
        integrated_revision.as_str()
    );
    assert_eq!(
        project["worktrees"][0]["conflict_paths"],
        serde_json::json!(["src/main.rs"])
    );
    assert_eq!(project["decisions"][0]["session_id"], child_text.as_str());
    assert_eq!(
        project["decisions"][0]["run_id"],
        run_id.to_string().as_str()
    );
    assert_eq!(project["decisions"][0]["run_state"], "awaiting_approval");
    assert_eq!(project["decisions"][0]["kind"], "approval");
    assert_eq!(
        project["decisions"][0]["tool_call_id"],
        call.to_string().as_str()
    );
    assert_eq!(project["decisions"][0]["tool_name"], tool_name.as_str());
    assert_eq!(envelope["data"]["verdict"], "blocked");
    assert_eq!(
        envelope["data"]["declaration"],
        format!("blocked: awaiting_approval {run_id}")
    );
    assert_eq!(envelope["data"]["run"]["id"], run_id.to_string().as_str());
    assert_eq!(
        envelope["data"]["pending_tool_call"],
        call.to_string().as_str()
    );
}

#[test]
fn status_tree_reports_a_question_decision() {
    let fixture = fixture();
    let root = fixture.session.id;
    let child = create_session(&fixture.control, fixture.workspace.id, "question child").id;
    pin_default_policy(&fixture.control, child);
    start_run(&fixture.control, child);
    let mut projection = await_run_state(&fixture.control, child, AgentRunState::AwaitingApproval);
    // A question blocks a run the same way an approval does, so the real
    // projection carries the other blocked shape with one rewrite instead of
    // provoking a provider question.
    let active = projection.active_run.as_mut().expect("a run");
    active.pending_approval = None;
    active.pending_input = Some("which branch should I use?".to_owned());
    active.run.state = AgentRunState::NeedsInput;
    projection.session.state = AgentSessionState::NeedsInput;
    let child_run = projection.active_run.as_ref().expect("a run").run.id;

    let mut blocked_project = fake_project(root, &[root, child], Vec::new());
    blocked_project.agents[0].state = AgentSessionState::Idle;
    blocked_project.agents[1].state = AgentSessionState::NeedsInput;
    // A snapshot that does not list the requested session still reports its own
    // decision, which is the second half of this test.
    let mut unlisted_project = fake_project(root, &[root], Vec::new());
    unlisted_project.agents[0].state = AgentSessionState::Idle;
    let doctored = projection.clone();
    let mut client =
        Wrapped::new(InProcessClient::new(&fixture.backend)).after(move |request, response| {
            match request {
                ClientRequest::Project(ProjectRequest::GetProjectSnapshotForSession {
                    session_id,
                }) if *session_id == root => {
                    response.result = Ok(ServerResponse::Project(
                        ProjectResponse::ProjectSnapshot(blocked_project.clone()),
                    ));
                }
                ClientRequest::Project(ProjectRequest::GetProjectSnapshotForSession {
                    session_id,
                }) if *session_id == child => {
                    response.result = Ok(ServerResponse::Project(
                        ProjectResponse::ProjectSnapshot(unlisted_project.clone()),
                    ));
                }
                ClientRequest::Session(SessionRequest::GetAgentSessionSnapshotMetadata {
                    session_id,
                    ..
                }) if *session_id == child => {
                    response.result = Ok(ServerResponse::Session(
                        SessionResponse::AgentSessionSnapshot(Box::new(doctored.clone())),
                    ));
                }
                _ => {}
            }
        });

    // The requested root has no run, so the decision of the child it does not
    // own comes from the one extra metadata read.
    let root_text = root.to_string();
    let tree = run(&mut client, &["status", "--session", &root_text, "--tree"]);
    assert_eq!(tree.code, exit::OK, "{}", tree.stderr);
    let expected = [
        format!("session {root} idle {}", fixture.session.name),
        format!("project {root} blocked"),
        format!("root {root}"),
        format!("agent {root} 1 idle done -"),
        format!("agent {child} 2 needs_input blocked -"),
        format!("decision question {child} which branch should I use?"),
    ]
    .join("\n");
    assert_eq!(tree.stdout, format!("{expected}\n"));
    let json = run(
        &mut client,
        &["status", "--session", &root_text, "--tree", "--json"],
    );
    assert_eq!(json.code, exit::OK, "{}", json.stderr);
    let envelope = json_envelope(&json);
    let decisions = &envelope["data"]["project"]["decisions"];
    assert_eq!(envelope["data"]["project"]["verdict"], "blocked");
    assert_eq!(decisions[0]["kind"], "question");
    assert_eq!(decisions[0]["session_id"], child.to_string().as_str());
    assert_eq!(decisions[0]["run_id"], child_run.to_string().as_str());
    assert_eq!(decisions[0]["run_state"], "needs_input");
    assert_eq!(decisions[0]["question"], "which branch should I use?");

    // A snapshot that does not list the requested session reports its own
    // decision after the listed agents.
    let child_text = child.to_string();
    let tree = run(&mut client, &["status", "--session", &child_text, "--tree"]);
    assert_eq!(tree.code, exit::OK, "{}", tree.stderr);
    let expected = [
        format!("session {child} needs_input question child"),
        format!("run {child_run} needs_input"),
        format!("project {root} done"),
        format!("root {root}"),
        format!("agent {root} 1 idle done -"),
        format!("decision question {child} which branch should I use?"),
    ]
    .join("\n");
    assert_eq!(tree.stdout, format!("{expected}\n"));
    let json = run(
        &mut client,
        &["status", "--session", &child_text, "--tree", "--json"],
    );
    let envelope = json_envelope(&json);
    let decisions = &envelope["data"]["project"]["decisions"];
    assert_eq!(decisions.as_array().unwrap().len(), 1);
    assert_eq!(decisions[0]["session_id"], child_text.as_str());
    assert_eq!(decisions[0]["kind"], "question");
}

#[test]
fn status_tree_prints_dashes_for_an_unintegrated_worktree() {
    let fixture = fixture();
    let root = fixture.session.id;
    let child = create_session(&fixture.control, fixture.workspace.id, "fresh child").id;
    let mut project = fake_project(root, &[root], Vec::new());
    project.agents[0].state = AgentSessionState::Idle;
    project.worktrees = vec![fake_worktree(
        root,
        child,
        "wt-2",
        "project-worktrees/fresh",
    )];
    let base_revision = project.worktrees[0].base_revision.clone();

    // A worktree with no result or integrated revision prints a dash for each,
    // and an empty conflict list prints no conflict line.
    let mut client = with_project_snapshot(
        Wrapped::new(InProcessClient::new(&fixture.backend)),
        root,
        project,
    );
    let root_text = root.to_string();
    let tree = run(&mut client, &["status", "--session", &root_text, "--tree"]);
    assert_eq!(tree.code, exit::OK, "{}", tree.stderr);
    let expected = [
        format!("session {root} idle {}", fixture.session.name),
        format!("project {root} done"),
        format!("root {root}"),
        format!("agent {root} 1 idle done -"),
        format!("worktree wt-2 ready project-worktrees/fresh {base_revision} - -"),
    ]
    .join("\n");
    assert_eq!(tree.stdout, format!("{expected}\n"));

    let json = run(
        &mut client,
        &["status", "--session", &root_text, "--tree", "--json"],
    );
    assert_eq!(json.code, exit::OK, "{}", json.stderr);
    let envelope = json_envelope(&json);
    let worktree = &envelope["data"]["project"]["worktrees"][0];
    assert_eq!(worktree["result_revision"], serde_json::Value::Null);
    assert_eq!(worktree["integrated_revision"], serde_json::Value::Null);
    assert_eq!(worktree["conflict_paths"], serde_json::json!([]));
}

#[test]
fn status_tree_fails_when_the_project_read_fails() {
    let fixture = fixture();
    let session_text = fixture.session.id.to_string();
    let mut client = Wrapped::new(InProcessClient::new(&fixture.backend)).fail_on(|request| {
        matches!(request, ClientRequest::Project(_)).then(|| {
            loom_core::LoomError::new(
                loom_core::ErrorCode::Internal,
                "the connection dropped",
                true,
            )
        })
    });

    // The verb reports the failed read as its own failure and prints nothing,
    // because no line is written before the request it reports succeeded.
    let tree = run(
        &mut client,
        &["status", "--session", &session_text, "--tree"],
    );
    assert_eq!(tree.code, exit::UNREACHABLE, "{}", tree.stderr);
    assert_eq!(tree.stdout, "");
    assert!(
        tree.stderr.contains("connection dropped"),
        "{}",
        tree.stderr
    );
}

#[test]
fn delete_removes_an_archived_session_and_refuses_a_live_one() {
    let (fixture, directory) = persistent_fixture("delete-removes");
    let session = fixture.session.id;
    archive_session(&fixture.control, session);
    let mut client = InProcessClient::new(&fixture.backend);

    let deleted = run(&mut client, &["delete", &session.to_string()]);
    assert_eq!(deleted.code, exit::OK, "{}", deleted.stderr);
    assert_eq!(deleted.stdout, format!("deleted {session}\n"));
    let gone = run(&mut client, &["get", &session.to_string()]);
    assert_eq!(gone.code, exit::GONE, "{}", gone.stderr);

    // A second session is not archived, so the worker refuses it and its
    // durable state is unchanged.
    let live = create_session(&fixture.control, fixture.workspace.id, "live session");
    let refused = run(&mut client, &["delete", &live.id.to_string()]);
    assert_eq!(refused.code, exit::GONE, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("invalid_state"),
        "{}",
        refused.stderr
    );
    assert_eq!(
        session_snapshot(&fixture.control, live.id).state,
        AgentSessionState::Idle
    );

    // A force deletion with no worktree still says explicitly that it discarded
    // none, so an operator can tell a clean removal from an unperformed one.
    archive_session(&fixture.control, live.id);
    let forced = run(
        &mut client,
        &["delete", &live.id.to_string(), "--force", "--yes"],
    );
    assert_eq!(forced.code, exit::OK, "{}", forced.stderr);
    assert_eq!(
        forced.stdout,
        format!("deleted {}\ndiscarded 0 worktrees\n", live.id)
    );
    drop(client);
    close_persistent_fixture(fixture, directory);
}

#[test]
fn delete_requires_yes_before_a_cascade_or_a_force_discard() {
    let (fixture, directory) = persistent_fixture("delete-yes");
    let session = fixture.session.id;
    archive_session(&fixture.control, session);

    // `--force` without `--yes` is refused before a connection is even used.
    let mut offline = OfflineClient;
    let refused = run(&mut offline, &["delete", &session.to_string(), "--force"]);
    assert_eq!(refused.code, exit::USAGE, "{}", refused.stderr);
    assert!(refused.stderr.contains("--yes"), "{}", refused.stderr);

    // A project root that cascades is refused after the read-only preflight and
    // before the delete request: the fail hook would turn a sent delete into an
    // internal error.
    let child = AgentSessionId::new();
    let project = fake_project(session, &[session, child], Vec::new());
    let mut cascade = with_project_snapshot(
        Wrapped::new(InProcessClient::new(&fixture.backend)),
        session,
        project.clone(),
    )
    .fail_on(|request| {
        matches!(
            request,
            ClientRequest::Session(SessionRequest::DeleteAgentSession { .. })
        )
        .then(|| {
            loom_core::LoomError::new(
                loom_core::ErrorCode::Internal,
                "the delete request reached the worker",
                false,
            )
        })
    });
    let refused = run(&mut cascade, &["delete", &session.to_string()]);
    assert_eq!(refused.code, exit::USAGE, "{}", refused.stderr);
    assert!(refused.stderr.contains("2 sessions"), "{}", refused.stderr);
    assert!(refused.stderr.contains("--yes"), "{}", refused.stderr);

    // With `--yes` the same cascade proceeds and names every session it removed.
    let mut confirmed = with_project_snapshot(
        Wrapped::new(InProcessClient::new(&fixture.backend)),
        session,
        project,
    );
    let deleted = run(
        &mut confirmed,
        &["delete", &session.to_string(), "--yes", "--json"],
    );
    assert_eq!(deleted.code, exit::OK, "{}", deleted.stderr);
    let envelope = json_envelope(&deleted);
    assert_eq!(envelope["verb"], "delete");
    assert_eq!(envelope["data"]["force"], false);
    assert_eq!(
        envelope["data"]["deleted_sessions"],
        serde_json::json!([session.to_string(), child.to_string()])
    );
    drop(cascade);
    drop(confirmed);
    close_persistent_fixture(fixture, directory);
}

#[test]
fn delete_force_reports_the_worktrees_it_discards() {
    let (fixture, directory) = persistent_fixture("delete-force");
    let session = fixture.session.id;
    archive_session(&fixture.control, session);
    let child = AgentSessionId::new();
    let project = fake_project(
        session,
        &[session],
        vec![fake_worktree(session, child, "wt-1", "repo")],
    );
    let mut client = with_project_snapshot(
        Wrapped::new(InProcessClient::new(&fixture.backend)),
        session,
        project,
    );

    let deleted = run(
        &mut client,
        &["delete", &session.to_string(), "--force", "--yes"],
    );
    assert_eq!(deleted.code, exit::OK, "{}", deleted.stderr);
    assert_eq!(
        deleted.stdout,
        format!("deleted {session}\ndiscarded worktree wt-1 repo\n")
    );

    let again = run(
        &mut client,
        &["delete", &session.to_string(), "--force", "--yes", "--json"],
    );
    assert_eq!(again.code, exit::GONE, "{}", again.stderr);
    drop(client);
    close_persistent_fixture(fixture, directory);
}

#[test]
fn the_parser_covers_the_operator_verbs() {
    let session = AgentSessionId::new().to_string();

    assert!(matches!(invocation(&["node"]).verb, Verb::Node));
    assert!(matches!(
        invocation(&["retention", "get"]).verb,
        Verb::RetentionGet
    ));
    let Verb::RetentionSet(set) = invocation(&[
        "retention",
        "set",
        "--window",
        "14d",
        "--force-discard=true",
        "--sub-agent",
        "10m",
    ])
    .verb
    else {
        panic!("retention set parses as retention set");
    };
    assert_eq!(set.window_ms, Some(Some(1_209_600_000)));
    assert_eq!(set.force_discard_worktrees, Some(true));
    assert_eq!(set.sub_agent_ms, Some(Some(600_000)));

    let Verb::RetentionSet(off) = invocation(&["retention", "set", "--window=off"]).verb else {
        panic!("retention set parses as retention set");
    };
    assert_eq!(off.window_ms, Some(None));
    assert_eq!(off.force_discard_worktrees, None);
    assert_eq!(off.sub_agent_ms, None);

    assert!(matches!(
        invocation(&["delete", &session]).verb,
        Verb::Delete {
            force: false,
            yes: false,
            ..
        }
    ));
    assert!(matches!(
        invocation(&["delete", &session, "--force", "--yes"]).verb,
        Verb::Delete {
            force: true,
            yes: true,
            ..
        }
    ));

    assert!(parse_error(&["node", "extra"]).contains("takes no positional"));
    assert!(parse_error(&["retention"]).contains("requires 'get' or 'set'"));
    assert!(parse_error(&["retention", "nope"]).contains("get or set"));
    assert!(parse_error(&["retention", "get", "set"]).contains("one sub-verb"));
    assert!(parse_error(&["retention", "set"]).contains("requires --window"));
    assert!(parse_error(&["retention", "get", "--window", "1d"]).contains("does not apply"));
    assert!(parse_error(&["retention", "set", "--window", "soon"]).contains("30s, 5m, 12h, 14d"));
    assert!(parse_error(&["delete"]).contains("requires a <session-id>"));
    assert!(parse_error(&["delete", &session, &session]).contains("one session id"));
    assert!(parse_error(&["delete", &session, "--force"]).contains("pass --yes to confirm"));
}

#[test]
fn the_parser_covers_the_lifecycle_verbs() {
    let session = AgentSessionId::new().to_string();
    let task = TaskId::new().to_string();

    assert!(matches!(
        invocation(&["session", "archive", &session]).verb,
        Verb::SessionArchive { yes: false, .. }
    ));
    assert!(matches!(
        invocation(&["session", "archive", &session, "--yes"]).verb,
        Verb::SessionArchive { yes: true, .. }
    ));
    assert!(matches!(
        invocation(&["session", "delete", &session]).verb,
        Verb::SessionDelete {
            force: false,
            yes: false,
            ..
        }
    ));
    assert!(matches!(
        invocation(&["session", "delete", &session, "--force", "--yes"]).verb,
        Verb::SessionDelete {
            force: true,
            yes: true,
            ..
        }
    ));
    assert!(matches!(
        invocation(&["project", "archive", &session]).verb,
        Verb::ProjectArchive { .. }
    ));
    assert!(matches!(
        invocation(&["project", "delete", &session, "--yes"]).verb,
        Verb::ProjectDelete {
            force: false,
            yes: true,
            ..
        }
    ));

    let cleanup = |flag: &str| {
        let Verb::ProjectWorktreeCleanup {
            task: parsed_task,
            disposition,
            ..
        } = invocation(&[
            "project", "worktree", "cleanup", &session, "--task", &task, flag,
        ])
        .verb
        else {
            panic!("'project worktree cleanup' parses as its own verb");
        };
        assert_eq!(parsed_task.to_string(), task);
        disposition
    };
    assert_eq!(cleanup("--keep"), WorktreeDisposition::Keep);
    assert_eq!(cleanup("--remove"), WorktreeDisposition::Remove);
    assert!(matches!(
        invocation(&[
            "project",
            "worktree",
            "cleanup",
            &session,
            "--task",
            &task,
            "--discard",
            "--yes",
        ])
        .verb,
        Verb::ProjectWorktreeCleanup {
            disposition: WorktreeDisposition::Discard,
            ..
        }
    ));

    assert!(parse_error(&["session"]).contains("requires 'archive' or 'delete'"));
    assert!(parse_error(&["session", "nope"]).contains("use archive or delete"));
    assert!(parse_error(&["session", "archive"]).contains("requires a <session-id>"));
    assert!(parse_error(&["session", "archive", &session, &session]).contains("one session id"));
    assert!(
        parse_error(&["session", "delete", &session, "--force"]).contains("pass --yes to confirm")
    );
    assert!(
        parse_error(&["project", "delete", &session, "--force"]).contains("pass --yes to confirm")
    );
    assert!(
        parse_error(&["project"])
            .contains("requires 'archive', 'delete', 'worktree', or 'worktrees'")
    );
    assert!(
        parse_error(&["project", "nope"]).contains("use archive, delete, worktree, or worktrees")
    );
    assert!(
        parse_error(&["project", "worktree"])
            .contains("requires 'get', 'diff', 'integrate', or 'cleanup'")
    );
    assert!(
        parse_error(&["project", "worktree", "nope"])
            .contains("use get, diff, integrate, or cleanup")
    );
    assert!(
        parse_error(&["project", "worktree", "cleanup", &session, "--keep"])
            .contains("requires --task")
    );
    assert!(
        parse_error(&["project", "worktree", "cleanup", &session, "--task", &task])
            .contains("requires --keep, --remove, or --discard")
    );
    assert!(
        parse_error(&[
            "project", "worktree", "cleanup", &session, "--task", &task, "--keep", "--remove",
        ])
        .contains("accepts one of --keep, --remove, or --discard")
    );
    assert!(
        parse_error(&[
            "project",
            "worktree",
            "cleanup",
            &session,
            "--task",
            &task,
            "--discard",
        ])
        .contains("pass --yes to confirm")
    );
    assert!(parse_error(&["project", "archive", &session, "--yes"]).contains("does not apply"));
    assert!(
        parse_error(&[
            "project", "worktree", "cleanup", &session, "--task", "nope", "--keep"
        ])
        .contains("not a valid task id")
    );
}

#[test]
fn the_parser_covers_worktree_inspection_and_integration() {
    let session = AgentSessionId::new().to_string();
    let task = TaskId::new().to_string();
    let revision = "a".repeat(40);

    let Verb::ProjectWorktrees {
        session: parsed_session,
    } = invocation(&["project", "worktrees", &session]).verb
    else {
        panic!("'project worktrees' parses as its own verb");
    };
    assert_eq!(parsed_session.to_string(), session);

    for sub_verb in ["get", "diff"] {
        let verb = invocation(&["project", "worktree", sub_verb, &session, "--task", &task]).verb;
        match (sub_verb, verb) {
            (
                "get",
                Verb::ProjectWorktreeGet {
                    session: parsed_session,
                    task: parsed_task,
                },
            )
            | (
                "diff",
                Verb::ProjectWorktreeDiff {
                    session: parsed_session,
                    task: parsed_task,
                },
            ) => {
                assert_eq!(parsed_session.to_string(), session);
                assert_eq!(parsed_task.to_string(), task);
            }
            (_, other) => panic!("'{sub_verb}' parses as {other:?}"),
        }
    }

    // The expected parent revision is absent when the flag is, so the verb
    // falls back to the revision the record itself names.
    let Verb::ProjectWorktreeIntegrate {
        task: parsed_task,
        expected_parent_revision,
        ..
    } = invocation(&[
        "project",
        "worktree",
        "integrate",
        &session,
        "--task",
        &task,
    ])
    .verb
    else {
        panic!("'project worktree integrate' parses as its own verb");
    };
    assert_eq!(parsed_task.to_string(), task);
    assert_eq!(expected_parent_revision, None);
    let Verb::ProjectWorktreeIntegrate {
        expected_parent_revision,
        ..
    } = invocation(&[
        "project",
        "worktree",
        "integrate",
        &session,
        "--task",
        &task,
        "--expected-parent-revision",
        &revision,
    ])
    .verb
    else {
        panic!("'project worktree integrate' parses as its own verb");
    };
    assert_eq!(expected_parent_revision, Some(revision.clone()));

    // A missing session id, an extra positional word, and a flag the sub-verb
    // does not use are all usage errors.
    assert!(parse_error(&["project", "worktrees"]).contains("requires a <session-id>"));
    assert!(parse_error(&["project", "worktrees", &session, &session]).contains("one session id"));
    assert!(
        parse_error(&["project", "worktrees", &session, "--task", &task])
            .contains("does not apply")
    );
    for sub_verb in ["get", "diff", "integrate"] {
        assert!(
            parse_error(&["project", "worktree", sub_verb]).contains("requires a <session-id>"),
            "{sub_verb}"
        );
        assert!(
            parse_error(&["project", "worktree", sub_verb, &session]).contains("requires --task"),
            "{sub_verb}"
        );
        assert!(
            parse_error(&[
                "project", "worktree", sub_verb, &session, &session, "--task", &task
            ])
            .contains("one session id"),
            "{sub_verb}"
        );
    }
    assert!(
        parse_error(&[
            "project",
            "worktree",
            "get",
            &session,
            "--task",
            &task,
            "--expected-parent-revision",
            &revision,
        ])
        .contains("does not apply")
    );
    assert!(
        parse_error(&[
            "project",
            "worktree",
            "integrate",
            &session,
            "--task",
            "nope"
        ])
        .contains("not a valid task id")
    );
}

#[test]
fn session_archive_archives_one_session_and_keeps_its_worktrees() {
    let (fixture, directory) = persistent_fixture("session-archive");
    let session = fixture.session.id;
    let mut client = InProcessClient::new(&fixture.backend);

    let archived = run(&mut client, &["session", "archive", &session.to_string()]);
    assert_eq!(archived.code, exit::OK, "{}", archived.stderr);
    assert_eq!(
        archived.stdout,
        format!("archived {session}\nretained 0 worktrees\n")
    );
    assert_eq!(
        session_snapshot(&fixture.control, session).state,
        AgentSessionState::Archived
    );

    // Archiving is a state change, not a deletion: the record is still there,
    // and the worker refuses to archive it twice.
    let again = run(&mut client, &["session", "archive", &session.to_string()]);
    assert_eq!(again.code, exit::GONE, "{}", again.stderr);
    assert!(again.stderr.contains("invalid_state"), "{}", again.stderr);
    assert_eq!(
        session_snapshot(&fixture.control, session).state,
        AgentSessionState::Archived
    );
    drop(client);
    close_persistent_fixture(fixture, directory);
}

#[test]
fn session_archive_requires_yes_before_a_project_cascade() {
    let (fixture, directory) = persistent_fixture("archive-yes");
    let session = fixture.session.id;
    let child = AgentSessionId::new();
    let mut project = fake_project(
        session,
        &[session, child],
        vec![fake_worktree(session, child, "wt-1", "repo")],
    );
    // A live member is what the worker cascades to; an archived one is skipped.
    project.agents[1].state = AgentSessionState::Idle;

    // The preflight refuses before the archive request: the fail hook would turn
    // a sent request into an internal error.
    let mut cascade = with_project_snapshot(
        Wrapped::new(InProcessClient::new(&fixture.backend)),
        session,
        project.clone(),
    )
    .fail_on(|request| {
        matches!(
            request,
            ClientRequest::Session(SessionRequest::ArchiveAgentSession { .. })
        )
        .then(|| {
            loom_core::LoomError::new(
                loom_core::ErrorCode::Internal,
                "the archive request reached the worker",
                false,
            )
        })
    });
    let refused = run(&mut cascade, &["session", "archive", &session.to_string()]);
    assert_eq!(refused.code, exit::USAGE, "{}", refused.stderr);
    assert!(refused.stderr.contains("2 sessions"), "{}", refused.stderr);
    assert!(refused.stderr.contains("--yes"), "{}", refused.stderr);
    assert_eq!(
        session_snapshot(&fixture.control, session).state,
        AgentSessionState::Idle
    );

    // With --yes the same command archives the tree and names what it kept.
    let mut text = with_project_snapshot(
        Wrapped::new(InProcessClient::new(&fixture.backend)),
        session,
        project,
    );
    let printed = run(
        &mut text,
        &["session", "archive", &session.to_string(), "--yes"],
    );
    assert_eq!(printed.code, exit::OK, "{}", printed.stderr);
    assert_eq!(
        printed.stdout,
        format!("archived {session}\narchived {child}\nretained worktree wt-1 repo\n")
    );

    // The envelope states the same facts. It archives a session of its own, so
    // the run above does not have to be undone first.
    let second = create_session(&fixture.control, fixture.workspace.id, "second session");
    let mut second_project = fake_project(
        second.id,
        &[second.id, child],
        vec![fake_worktree(second.id, child, "wt-1", "repo")],
    );
    second_project.agents[1].state = AgentSessionState::Idle;
    let mut confirmed = with_project_snapshot(
        Wrapped::new(InProcessClient::new(&fixture.backend)),
        second.id,
        second_project,
    );
    let archived = run(
        &mut confirmed,
        &[
            "session",
            "archive",
            &second.id.to_string(),
            "--yes",
            "--json",
        ],
    );
    assert_eq!(archived.code, exit::OK, "{}", archived.stderr);
    let envelope = json_envelope(&archived);
    assert_eq!(envelope["verb"], "session archive");
    assert_eq!(envelope["data"]["session_id"], second.id.to_string());
    assert_eq!(envelope["data"]["cascade"], true);
    assert_eq!(
        envelope["data"]["archived_sessions"],
        serde_json::json!([second.id.to_string(), child.to_string()])
    );
    assert_eq!(
        envelope["data"]["retained_worktrees"],
        serde_json::json!([{"name": "wt-1", "path": "repo"}])
    );
    drop(cascade);
    drop(confirmed);
    drop(text);
    close_persistent_fixture(fixture, directory);
}

#[test]
fn session_archive_of_a_project_member_stays_on_that_session() {
    let (fixture, directory) = persistent_fixture("archive-member");
    let session = fixture.session.id;
    let root = AgentSessionId::new();
    // The member is not the root, so the worker archives only this session even
    // though the project owns another live member and a checkout for it.
    let mut project = fake_project(
        root,
        &[root, session],
        vec![fake_worktree(root, session, "wt-1", "repo")],
    );
    project.agents[0].state = AgentSessionState::Idle;
    let mut client = with_project_snapshot(
        Wrapped::new(InProcessClient::new(&fixture.backend)),
        session,
        project,
    );
    let archived = run(&mut client, &["session", "archive", &session.to_string()]);
    assert_eq!(archived.code, exit::OK, "{}", archived.stderr);
    assert_eq!(
        archived.stdout,
        format!("archived {session}\nretained worktree wt-1 repo\n")
    );
    drop(client);
    close_persistent_fixture(fixture, directory);
}

#[test]
fn session_delete_is_the_same_operation_as_delete() {
    let (fixture, directory) = persistent_fixture("session-delete");
    let first = fixture.session.id;
    let second = create_session(&fixture.control, fixture.workspace.id, "second session");
    archive_session(&fixture.control, first);
    archive_session(&fixture.control, second.id);
    let mut client = InProcessClient::new(&fixture.backend);

    let explicit = run(
        &mut client,
        &["session", "delete", &first.to_string(), "--json"],
    );
    assert_eq!(explicit.code, exit::OK, "{}", explicit.stderr);
    let envelope = json_envelope(&explicit);
    assert_eq!(envelope["verb"], "session delete");
    assert_eq!(
        envelope["data"]["deleted_sessions"],
        serde_json::json!([first.to_string()])
    );

    // The original spelling is the same operation, and keeps its own verb name.
    let alias = run(&mut client, &["delete", &second.id.to_string()]);
    assert_eq!(alias.code, exit::OK, "{}", alias.stderr);
    assert_eq!(alias.stdout, format!("deleted {}\n", second.id));

    // A rejected command line still names the verb the caller typed.
    let missing_id = run(&mut client, &["session", "archive", "--json"]);
    assert_eq!(missing_id.code, exit::USAGE, "{}", missing_id.stderr);
    assert_eq!(json_envelope(&missing_id)["verb"], "session");
    let unknown_sub_verb = run(&mut client, &["project", "nope", "--json"]);
    assert_eq!(
        unknown_sub_verb.code,
        exit::USAGE,
        "{}",
        unknown_sub_verb.stderr
    );
    assert_eq!(json_envelope(&unknown_sub_verb)["verb"], "project");
    drop(client);
    close_persistent_fixture(fixture, directory);
}

#[test]
fn project_archive_and_delete_require_a_project_root() {
    let (fixture, directory) = persistent_fixture("project-root");
    let session = fixture.session.id;
    let session_text = session.to_string();

    // A plain session is not a project root, so the project spelling refuses it
    // before sending any mutation.
    let mut plain = Wrapped::new(InProcessClient::new(&fixture.backend)).fail_on(|request| {
        matches!(
            request,
            ClientRequest::Session(SessionRequest::ArchiveAgentSession { .. })
                | ClientRequest::Session(SessionRequest::DeleteAgentSession { .. })
        )
        .then(|| {
            loom_core::LoomError::new(
                loom_core::ErrorCode::Internal,
                "a project mutation reached the worker",
                false,
            )
        })
    });
    let refused_archive = run(&mut plain, &["project", "archive", &session_text]);
    assert_eq!(
        refused_archive.code,
        exit::USAGE,
        "{}",
        refused_archive.stderr
    );
    assert!(
        refused_archive.stderr.contains("not a project root"),
        "{}",
        refused_archive.stderr
    );
    assert!(
        refused_archive.stderr.contains("session archive"),
        "{}",
        refused_archive.stderr
    );
    let refused_delete = run(&mut plain, &["project", "delete", &session_text, "--yes"]);
    assert_eq!(
        refused_delete.code,
        exit::USAGE,
        "{}",
        refused_delete.stderr
    );
    assert!(
        refused_delete.stderr.contains("session delete"),
        "{}",
        refused_delete.stderr
    );

    // A real project root is archived, and then deleted, through the project
    // spelling. Neither command needs --yes for the archive, because the verb
    // itself names the tree; the delete needs it because it is irreversible.
    let child = AgentSessionId::new();
    let mut project = fake_project(session, &[session, child], Vec::new());
    project.agents[1].state = AgentSessionState::Idle;
    let mut root = with_project_snapshot(
        Wrapped::new(InProcessClient::new(&fixture.backend)),
        session,
        project.clone(),
    );
    let archived = run(&mut root, &["project", "archive", &session_text]);
    assert_eq!(archived.code, exit::OK, "{}", archived.stderr);
    assert_eq!(
        archived.stdout,
        format!("archived {session}\narchived {child}\nretained 0 worktrees\n")
    );
    assert_eq!(
        session_snapshot(&fixture.control, session).state,
        AgentSessionState::Archived
    );

    let deleted = run(&mut root, &["project", "delete", &session_text, "--yes"]);
    assert_eq!(deleted.code, exit::OK, "{}", deleted.stderr);
    assert_eq!(
        deleted.stdout,
        format!("deleted {session}\ndeleted {child}\n")
    );
    let gone = run(&mut root, &["get", &session_text]);
    assert_eq!(gone.code, exit::GONE, "{}", gone.stderr);
    drop(root);
    drop(plain);
    close_persistent_fixture(fixture, directory);
}

/// A client whose project snapshot and cleanup answer are decided by the test.
fn cleanup_client(
    backend: &Arc<InProcessBackend>,
    session: AgentSessionId,
    project: ProjectSnapshot,
    answer: ProjectWorktreeRecord,
    sent: Rc<RefCell<Vec<ClientRequest>>>,
) -> Wrapped<InProcessClient> {
    Wrapped::new(InProcessClient::new(backend))
        .before(move |envelope| sent.borrow_mut().push(envelope.request.clone()))
        .after(move |request, response| match request {
            ClientRequest::Project(ProjectRequest::GetProjectSnapshotForSession { session_id })
                if *session_id == session =>
            {
                response.result = Ok(ServerResponse::Project(ProjectResponse::ProjectSnapshot(
                    project.clone(),
                )));
            }
            ClientRequest::Project(ProjectRequest::CleanupProjectChildWorktree { .. }) => {
                response.result = Ok(ServerResponse::Project(
                    ProjectResponse::ProjectChildWorktreeUpdated(Box::new(answer.clone())),
                ));
            }
            _ => {}
        })
}

#[test]
fn project_worktree_cleanup_reports_what_it_kept_or_removed() {
    let (fixture, directory) = persistent_fixture("worktree-cleanup");
    let session = fixture.session.id;
    let task = TaskId::new();
    let mut worktree = fake_worktree(session, AgentSessionId::new(), "wt-1", "repo");
    worktree.task_id = task;
    let project = fake_project(session, &[session], vec![worktree.clone()]);
    let session_text = session.to_string();
    let task_text = task.to_string();

    // A task without a worktree record is refused after the preflight read and
    // before any cleanup request.
    let missing_sent = Rc::new(RefCell::new(Vec::new()));
    let mut missing = cleanup_client(
        &fixture.backend,
        session,
        fake_project(session, &[session], Vec::new()),
        worktree.clone(),
        Rc::clone(&missing_sent),
    );
    let refused = run(
        &mut missing,
        &[
            "project",
            "worktree",
            "cleanup",
            &session_text,
            "--task",
            &task_text,
            "--keep",
        ],
    );
    assert_eq!(refused.code, exit::USAGE, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("no project worktree is recorded"),
        "{}",
        refused.stderr
    );
    assert!(
        !missing_sent.borrow().iter().any(|request| matches!(
            request,
            ClientRequest::Project(ProjectRequest::CleanupProjectChildWorktree { .. })
        )),
        "a refused cleanup must not reach the worker"
    );

    // --keep retains the checkout the snapshot records, and the request carries
    // the record's own project and manager, not what the caller typed.
    let kept_sent = Rc::new(RefCell::new(Vec::new()));
    let mut retained = worktree.clone();
    retained.status = ProjectWorktreeStatus::Retained;
    retained.cleanup_disposition = Some(ProjectWorktreeCleanupDisposition::Retain);
    let mut keep = cleanup_client(
        &fixture.backend,
        session,
        project.clone(),
        retained,
        Rc::clone(&kept_sent),
    );
    let kept = run(
        &mut keep,
        &[
            "project",
            "worktree",
            "cleanup",
            &session_text,
            "--task",
            &task_text,
            "--keep",
            "--json",
        ],
    );
    assert_eq!(kept.code, exit::OK, "{}", kept.stderr);
    let envelope = json_envelope(&kept);
    assert_eq!(envelope["verb"], "project worktree cleanup");
    assert_eq!(envelope["data"]["disposition"], "keep");
    assert_eq!(envelope["data"]["task_id"], task_text);
    assert_eq!(envelope["data"]["worktree"]["status"], "retained");
    let cleanup = kept_sent
        .borrow()
        .iter()
        .find_map(|request| match request {
            ClientRequest::Project(ProjectRequest::CleanupProjectChildWorktree {
                project_id,
                manager_session_id,
                task_id,
                disposition,
            }) => Some((*project_id, *manager_session_id, *task_id, *disposition)),
            _ => None,
        })
        .expect("a cleanup request reached the worker");
    assert_eq!(
        cleanup,
        (
            worktree.project_id,
            session,
            task,
            ProjectWorktreeCleanupDisposition::Retain
        )
    );

    // --remove reports the removed checkout, and --discard also reports what it
    // threw away, because that loss has nowhere else to be seen.
    for (flag, disposition, note) in [
        (
            "--remove",
            ProjectWorktreeCleanupDisposition::RemoveClean,
            None,
        ),
        (
            "--discard",
            ProjectWorktreeCleanupDisposition::DiscardChanges,
            Some(
                "the manager discarded this checkout, so its 2 uncommitted path(s) will not be landed",
            ),
        ),
    ] {
        let sent = Rc::new(RefCell::new(Vec::new()));
        let mut removed = worktree.clone();
        removed.status = ProjectWorktreeStatus::Removed;
        removed.cleanup_disposition = Some(disposition);
        removed.error = note.map(str::to_owned);
        let mut client = cleanup_client(
            &fixture.backend,
            session,
            project.clone(),
            removed,
            Rc::clone(&sent),
        );
        let mut arguments = vec![
            "project",
            "worktree",
            "cleanup",
            &session_text,
            "--task",
            &task_text,
            flag,
        ];
        if note.is_some() {
            arguments.push("--yes");
        }
        let outcome = run_owned(
            &mut client,
            &arguments
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>(),
        );
        assert_eq!(outcome.code, exit::OK, "{}", outcome.stderr);
        let mut expected = String::from("removed worktree wt-1 repo\n");
        if let Some(note) = note {
            expected.push_str(&format!("note {note}\n"));
        }
        assert_eq!(outcome.stdout, expected);
        let cleanup = sent
            .borrow()
            .iter()
            .find_map(|request| match request {
                ClientRequest::Project(ProjectRequest::CleanupProjectChildWorktree {
                    disposition,
                    ..
                }) => Some(*disposition),
                _ => None,
            })
            .expect("a cleanup request reached the worker");
        assert_eq!(cleanup, disposition);
    }
    drop(missing);
    drop(keep);
    close_persistent_fixture(fixture, directory);
}

/// What a test decides the review or integration answer is.
enum ProjectAnswer {
    /// The review of one checkout, as the worker reports it.
    Review {
        worktree: Box<ProjectWorktreeRecord>,
        status: Box<GitRepositoryStatus>,
        diff: Box<GitDiff>,
    },
    /// The updated record an integration reports.
    Updated(Box<ProjectWorktreeRecord>),
    /// The error the worker answers the verb with.
    Refused(loom_core::LoomError),
}

fn review_answer(
    worktree: &ProjectWorktreeRecord,
    status: &GitRepositoryStatus,
    diff: &GitDiff,
) -> ProjectAnswer {
    ProjectAnswer::Review {
        worktree: Box::new(worktree.clone()),
        status: Box::new(status.clone()),
        diff: Box::new(diff.clone()),
    }
}

/// A client whose project snapshot and review or integration answer are decided
/// by the test, recording every request it sent.
fn worktree_client(
    backend: &Arc<InProcessBackend>,
    session: AgentSessionId,
    project: ProjectSnapshot,
    answer: ProjectAnswer,
    sent: Rc<RefCell<Vec<ClientRequest>>>,
) -> Wrapped<InProcessClient> {
    Wrapped::new(InProcessClient::new(backend))
        .before(move |envelope| sent.borrow_mut().push(envelope.request.clone()))
        .after(move |request, response| match request {
            ClientRequest::Project(ProjectRequest::GetProjectSnapshotForSession { session_id })
                if *session_id == session =>
            {
                response.result = Ok(ServerResponse::Project(ProjectResponse::ProjectSnapshot(
                    project.clone(),
                )));
            }
            ClientRequest::Project(ProjectRequest::GetProjectChildReview { .. }) => {
                if let ProjectAnswer::Review {
                    worktree,
                    status,
                    diff,
                } = &answer
                {
                    response.result = Ok(ServerResponse::Project(
                        ProjectResponse::ProjectChildReview {
                            worktree: worktree.clone(),
                            status: (**status).clone(),
                            diff: (**diff).clone(),
                        },
                    ));
                }
            }
            ClientRequest::Project(ProjectRequest::IntegrateProjectChild { .. }) => match &answer {
                ProjectAnswer::Updated(worktree) => {
                    response.result = Ok(ServerResponse::Project(
                        ProjectResponse::ProjectChildWorktreeUpdated(worktree.clone()),
                    ));
                }
                ProjectAnswer::Refused(error) => response.result = Err(error.clone()),
                _ => {}
            },
            _ => {}
        })
}

/// The review requests a test client sent, as their project, manager, and task.
fn review_requests(
    sent: &Rc<RefCell<Vec<ClientRequest>>>,
) -> Vec<(ProjectId, AgentSessionId, TaskId)> {
    sent.borrow()
        .iter()
        .filter_map(|request| match request {
            ClientRequest::Project(ProjectRequest::GetProjectChildReview {
                project_id,
                manager_session_id,
                task_id,
            }) => Some((*project_id, *manager_session_id, *task_id)),
            _ => None,
        })
        .collect()
}

/// The integration requests a test client sent, with the revision each expected.
fn integrate_requests(
    sent: &Rc<RefCell<Vec<ClientRequest>>>,
) -> Vec<(ProjectId, AgentSessionId, TaskId, String)> {
    sent.borrow()
        .iter()
        .filter_map(|request| match request {
            ClientRequest::Project(ProjectRequest::IntegrateProjectChild {
                project_id,
                manager_session_id,
                task_id,
                expected_parent_revision,
            }) => Some((
                *project_id,
                *manager_session_id,
                *task_id,
                expected_parent_revision.clone(),
            )),
            _ => None,
        })
        .collect()
}

/// One changed file of a review.
fn fake_file(path: &str, status: GitFileStatusKind, conflicted: bool) -> GitFileStatus {
    GitFileStatus {
        path: path.to_owned(),
        original_path: None,
        index: status,
        worktree: status,
        conflicted,
        index_additions: 0,
        index_deletions: 0,
        worktree_additions: 0,
        worktree_deletions: 0,
    }
}

/// The status of a review: two changed paths, the second one conflicted.
fn fake_status() -> GitRepositoryStatus {
    GitRepositoryStatus {
        root: "repo".to_owned(),
        branch: Some("loom/test".to_owned()),
        head: Some("c".repeat(40)),
        files: vec![
            fake_file("src/lib.rs", GitFileStatusKind::Modified, false),
            fake_file("README.md", GitFileStatusKind::Deleted, true),
        ],
        conflicts: Vec::new(),
        clean: false,
        captured_at: Timestamp::now(),
    }
}

/// A diff with one hunk holding a context, two added, and one removed line.
fn fake_diff() -> GitDiff {
    GitDiff {
        path: None,
        staged: false,
        patch: String::new(),
        binary: false,
        hunks: vec![GitDiffHunk {
            old_start: 1,
            old_lines: 2,
            new_start: 1,
            new_lines: 3,
            lines: vec![
                GitDiffLine {
                    kind: GitDiffLineKind::Context,
                    old_line: Some(1),
                    new_line: Some(1),
                    content: "fn main() {".to_owned(),
                },
                GitDiffLine {
                    kind: GitDiffLineKind::Added,
                    old_line: None,
                    new_line: Some(2),
                    content: "\tprintln!(\"hi\");".to_owned(),
                },
                GitDiffLine {
                    kind: GitDiffLineKind::Added,
                    old_line: None,
                    new_line: Some(3),
                    content: "let value = 1;".to_owned(),
                },
                GitDiffLine {
                    kind: GitDiffLineKind::Removed,
                    old_line: Some(2),
                    new_line: None,
                    content: "old();".to_owned(),
                },
            ],
        }],
        truncated: false,
    }
}

#[test]
fn project_worktree_cleanup_states_every_status_in_text() {
    let (fixture, directory) = persistent_fixture("worktree-status");
    let session = fixture.session.id;
    let task = TaskId::new();
    let mut worktree = fake_worktree(session, AgentSessionId::new(), "wt-1", "repo");
    worktree.task_id = task;
    let project = fake_project(session, &[session], vec![worktree.clone()]);
    let task_text = task.to_string();

    let cases = [
        (ProjectWorktreeStatus::Retained, "retained"),
        (ProjectWorktreeStatus::Removed, "removed"),
        (ProjectWorktreeStatus::Creating, "creating"),
        (ProjectWorktreeStatus::Ready, "ready"),
        (ProjectWorktreeStatus::Stale, "stale"),
        (ProjectWorktreeStatus::Conflict, "conflict"),
        (ProjectWorktreeStatus::Integrating, "integrating"),
        (ProjectWorktreeStatus::Integrated, "integrated"),
        (ProjectWorktreeStatus::RecoveryRequired, "recovery_required"),
        (ProjectWorktreeStatus::CleanupPending, "cleanup_pending"),
    ];
    for (status, word) in cases {
        let mut answer = worktree.clone();
        answer.status = status;
        answer.cleanup_disposition = Some(ProjectWorktreeCleanupDisposition::RemoveClean);
        let mut client = cleanup_client(
            &fixture.backend,
            session,
            project.clone(),
            answer,
            Rc::new(RefCell::new(Vec::new())),
        );
        let outcome = run(
            &mut client,
            &[
                "project",
                "worktree",
                "cleanup",
                &session.to_string(),
                "--task",
                &task_text,
                "--remove",
            ],
        );
        assert_eq!(outcome.code, exit::OK, "{}", outcome.stderr);
        let expected = match status {
            ProjectWorktreeStatus::Retained | ProjectWorktreeStatus::Removed => {
                format!("{word} worktree wt-1 repo\n")
            }
            _ => format!("worktree wt-1 repo {word}\n"),
        };
        assert_eq!(outcome.stdout, expected);
    }
    close_persistent_fixture(fixture, directory);
}

#[test]
fn project_worktrees_lists_the_snapshot_records() {
    let (fixture, directory) = persistent_fixture("worktrees-list");
    let session = fixture.session.id;
    let child = AgentSessionId::new();
    let first = fake_worktree(session, child, "wt-1", "repo");
    let mut second = fake_worktree(session, child, "wt-2", "repo-b");
    second.status = ProjectWorktreeStatus::Stale;
    let project = fake_project(session, &[session], vec![first.clone(), second.clone()]);
    let session_text = session.to_string();
    let sent = Rc::new(RefCell::new(Vec::new()));
    let mut client = worktree_client(
        &fixture.backend,
        session,
        project.clone(),
        ProjectAnswer::Updated(Box::new(first.clone())),
        Rc::clone(&sent),
    );

    let listed = run(&mut client, &["project", "worktrees", &session_text]);
    assert_eq!(listed.code, exit::OK, "{}", listed.stderr);
    assert_eq!(
        listed.stdout,
        format!(
            "project {}\nworktrees 2\nworktree {} ready wt-1 repo\nworktree {} stale wt-2 repo-b\n",
            project.project_id, first.task_id, second.task_id
        )
    );

    let payload = run(
        &mut client,
        &["project", "worktrees", &session_text, "--json"],
    );
    assert_eq!(payload.code, exit::OK, "{}", payload.stderr);
    let envelope = json_envelope(&payload);
    assert_eq!(envelope["verb"], "project worktrees");
    assert_eq!(envelope["data"]["session_id"], session_text);
    assert_eq!(
        envelope["data"]["project_id"],
        project.project_id.to_string()
    );
    assert_eq!(envelope["data"]["root_session_id"], session_text);
    assert_eq!(envelope["data"]["worktrees"][0]["worktree_name"], "wt-1");
    assert_eq!(envelope["data"]["worktrees"][1]["status"], "stale");
    assert_eq!(envelope["data"]["worktrees"][1]["branch_name"], "loom/test");
    // Listing is one read per run: the handshake and the snapshot alone.
    assert_eq!(sent.borrow().len(), 4, "two runs of one read each");
    drop(client);

    // A project with no checkout says so explicitly.
    let empty = fake_project(session, &[session], Vec::new());
    let mut empty_client = worktree_client(
        &fixture.backend,
        session,
        empty.clone(),
        ProjectAnswer::Updated(Box::new(first.clone())),
        Rc::new(RefCell::new(Vec::new())),
    );
    let none = run(&mut empty_client, &["project", "worktrees", &session_text]);
    assert_eq!(none.code, exit::OK, "{}", none.stderr);
    assert_eq!(
        none.stdout,
        format!("project {}\nworktrees 0\n", empty.project_id)
    );
    drop(empty_client);
    close_persistent_fixture(fixture, directory);
}

#[test]
fn project_worktree_get_reports_the_durable_record() {
    let (fixture, directory) = persistent_fixture("worktree-get");
    let session = fixture.session.id;
    let child = AgentSessionId::new();
    let task = TaskId::new();
    let result_revision = "a".repeat(40);
    let integrated_revision = "b".repeat(40);
    let mut worktree = fake_worktree(session, child, "wt-1", "repo");
    worktree.task_id = task;
    worktree.result_revision = Some(result_revision.clone());
    worktree.integrated_revision = Some(integrated_revision.clone());
    worktree.conflict_paths = vec!["src/lib.rs".to_owned(), "README.md".to_owned()];
    worktree.cleanup_disposition = Some(ProjectWorktreeCleanupDisposition::DiscardChanges);
    worktree.error = Some("the manager discarded this checkout".to_owned());
    let project = fake_project(session, &[session], vec![worktree.clone()]);
    let session_text = session.to_string();
    let task_text = task.to_string();

    let sent = Rc::new(RefCell::new(Vec::new()));
    let mut client = worktree_client(
        &fixture.backend,
        session,
        project.clone(),
        ProjectAnswer::Updated(Box::new(worktree.clone())),
        Rc::clone(&sent),
    );
    let reported = run(
        &mut client,
        &[
            "project",
            "worktree",
            "get",
            &session_text,
            "--task",
            &task_text,
        ],
    );
    assert_eq!(reported.code, exit::OK, "{}", reported.stderr);
    assert_eq!(
        reported.stdout,
        format!(
            "worktree wt-1 repo\n\
             task {task}\n\
             project {project}\n\
             owner {owner}\n\
             child {child}\n\
             parent-repository {parent_repository}\n\
             child-repository {child_repository}\n\
             branch loom/test\n\
             parent-revision {base}\n\
             child-revision {result_revision}\n\
             integrated-revision {integrated_revision}\n\
             status ready\n\
             conflict src/lib.rs\n\
             conflict README.md\n\
             cleanup discard_changes\n\
             note {note}\n",
            task = worktree.task_id,
            project = worktree.project_id,
            owner = worktree.parent_session_id,
            child = worktree.child_session_id,
            parent_repository = worktree.parent_repository_id,
            child_repository = worktree.child_repository_id,
            base = worktree.base_revision,
            note = worktree.error.clone().unwrap_or_default(),
        )
    );

    // The record is reported from the snapshot alone, so nothing opens the
    // checkout.
    assert!(review_requests(&sent).is_empty());
    assert!(integrate_requests(&sent).is_empty());

    let payload = run(
        &mut client,
        &[
            "project",
            "worktree",
            "get",
            &session_text,
            "--task",
            &task_text,
            "--json",
        ],
    );
    assert_eq!(payload.code, exit::OK, "{}", payload.stderr);
    let envelope = json_envelope(&payload);
    assert_eq!(envelope["verb"], "project worktree get");
    assert_eq!(envelope["data"]["session_id"], session_text);
    assert_eq!(envelope["data"]["task_id"], task_text);
    assert_eq!(envelope["data"]["worktree"]["status"], "ready");
    assert_eq!(
        envelope["data"]["worktree"]["conflict_paths"][1],
        "README.md"
    );
    assert_eq!(
        envelope["data"]["worktree"]["cleanup_disposition"],
        "discard_changes"
    );
    drop(client);

    // A bare record states the absent revisions and disposition as `-`.
    let mut bare = fake_worktree(session, child, "wt-1", "repo");
    bare.task_id = task;
    let mut bare_client = worktree_client(
        &fixture.backend,
        session,
        fake_project(session, &[session], vec![bare.clone()]),
        ProjectAnswer::Updated(Box::new(bare.clone())),
        Rc::new(RefCell::new(Vec::new())),
    );
    let bare_outcome = run(
        &mut bare_client,
        &[
            "project",
            "worktree",
            "get",
            &session_text,
            "--task",
            &task_text,
        ],
    );
    assert_eq!(bare_outcome.code, exit::OK, "{}", bare_outcome.stderr);
    assert_eq!(
        bare_outcome.stdout,
        format!(
            "worktree wt-1 repo\n\
             task {task}\n\
             project {project}\n\
             owner {owner}\n\
             child {child}\n\
             parent-repository {parent_repository}\n\
             child-repository {child_repository}\n\
             branch loom/test\n\
             parent-revision {base}\n\
             child-revision -\n\
             integrated-revision -\n\
             status ready\n\
             cleanup -\n",
            task = bare.task_id,
            project = bare.project_id,
            owner = bare.parent_session_id,
            child = bare.child_session_id,
            parent_repository = bare.parent_repository_id,
            child_repository = bare.child_repository_id,
            base = bare.base_revision,
        )
    );
    drop(bare_client);

    // A task without a record is a usage error after the snapshot, before any
    // other request.
    let missing_sent = Rc::new(RefCell::new(Vec::new()));
    let mut missing = worktree_client(
        &fixture.backend,
        session,
        fake_project(session, &[session], Vec::new()),
        ProjectAnswer::Updated(Box::new(worktree.clone())),
        Rc::clone(&missing_sent),
    );
    let refused = run(
        &mut missing,
        &[
            "project",
            "worktree",
            "get",
            &session_text,
            "--task",
            &task_text,
        ],
    );
    assert_eq!(refused.code, exit::USAGE, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("no project worktree is recorded"),
        "{}",
        refused.stderr
    );
    assert_eq!(
        missing_sent.borrow().len(),
        2,
        "a refused get stops after the snapshot"
    );
    drop(missing);
    close_persistent_fixture(fixture, directory);
}

#[test]
fn project_worktree_diff_reports_the_worker_review() {
    let (fixture, directory) = persistent_fixture("worktree-diff");
    let session = fixture.session.id;
    let child = AgentSessionId::new();
    let task = TaskId::new();
    let mut worktree = fake_worktree(session, child, "wt-1", "repo");
    worktree.task_id = task;
    let project = fake_project(session, &[session], vec![worktree.clone()]);
    let status = fake_status();
    let diff = fake_diff();
    let session_text = session.to_string();
    let task_text = task.to_string();

    let sent = Rc::new(RefCell::new(Vec::new()));
    let mut client = worktree_client(
        &fixture.backend,
        session,
        project.clone(),
        review_answer(&worktree, &status, &diff),
        Rc::clone(&sent),
    );
    let reviewed = run(
        &mut client,
        &[
            "project",
            "worktree",
            "diff",
            &session_text,
            "--task",
            &task_text,
        ],
    );
    assert_eq!(reviewed.code, exit::OK, "{}", reviewed.stderr);
    assert_eq!(
        reviewed.stdout,
        format!(
            "worktree wt-1 repo\n\
             branch loom/test\n\
             head {head}\n\
             parent-revision {base}\n\
             status ready\n\
             diffstat 2 2 1\n\
             changed modified src/lib.rs\n\
             changed deleted README.md\n\
             diff context fn main() {{\n\
             diff added \\tprintln!(\"hi\");\n\
             diff added let value = 1;\n\
             diff removed old();\n",
            head = status.head.clone().unwrap_or_default(),
            base = worktree.base_revision,
        )
    );

    // The review request carries the record's own project and manager.
    assert_eq!(
        review_requests(&sent),
        vec![(worktree.project_id, session, task)]
    );

    let payload = run(
        &mut client,
        &[
            "project",
            "worktree",
            "diff",
            &session_text,
            "--task",
            &task_text,
            "--json",
        ],
    );
    assert_eq!(payload.code, exit::OK, "{}", payload.stderr);
    let envelope = json_envelope(&payload);
    assert_eq!(envelope["verb"], "project worktree diff");
    assert_eq!(envelope["data"]["task_id"], task_text);
    assert_eq!(envelope["data"]["worktree"]["worktree_name"], "wt-1");
    assert_eq!(envelope["data"]["status"]["branch"], "loom/test");
    assert_eq!(
        envelope["data"]["diffstat"],
        serde_json::json!({"files": 2, "added": 2, "removed": 1})
    );
    assert_eq!(envelope["data"]["changed_paths"][0]["path"], "src/lib.rs");
    assert_eq!(envelope["data"]["changed_paths"][0]["status"], "modified");
    assert_eq!(envelope["data"]["changed_paths"][1]["status"], "deleted");
    assert_eq!(envelope["data"]["changed_paths"][1]["conflicted"], true);
    assert_eq!(
        envelope["data"]["diff"]["hunks"][0]["lines"]
            .as_array()
            .map(Vec::len),
        Some(4)
    );
    drop(client);

    // The markers say what the worker's bound did, each reachable on its own.
    for (case, marker) in [
        (
            GitDiff {
                binary: true,
                hunks: Vec::new(),
                ..fake_diff()
            },
            "diff binary\n",
        ),
        (
            GitDiff {
                truncated: true,
                ..fake_diff()
            },
            "diff truncated\n",
        ),
        (
            GitDiff {
                hunks: Vec::new(),
                ..fake_diff()
            },
            "diff none\n",
        ),
    ] {
        let mut bounded = worktree_client(
            &fixture.backend,
            session,
            project.clone(),
            review_answer(&worktree, &status, &case),
            Rc::new(RefCell::new(Vec::new())),
        );
        let outcome = run(
            &mut bounded,
            &[
                "project",
                "worktree",
                "diff",
                &session_text,
                "--task",
                &task_text,
            ],
        );
        assert_eq!(outcome.code, exit::OK, "{}", outcome.stderr);
        assert!(outcome.stdout.contains(marker), "{}", outcome.stdout);
    }

    // A task without a record is refused before the review request.
    let missing_sent = Rc::new(RefCell::new(Vec::new()));
    let mut missing = worktree_client(
        &fixture.backend,
        session,
        fake_project(session, &[session], Vec::new()),
        review_answer(&worktree, &status, &diff),
        Rc::clone(&missing_sent),
    );
    let refused = run(
        &mut missing,
        &[
            "project",
            "worktree",
            "diff",
            &session_text,
            "--task",
            &task_text,
        ],
    );
    assert_eq!(refused.code, exit::USAGE, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("no project worktree is recorded"),
        "{}",
        refused.stderr
    );
    assert!(review_requests(&missing_sent).is_empty());
    drop(missing);
    close_persistent_fixture(fixture, directory);
}

#[test]
fn project_worktree_integrate_sends_one_request_and_never_retries_a_conflict() {
    let (fixture, directory) = persistent_fixture("worktree-integrate");
    let session = fixture.session.id;
    let child = AgentSessionId::new();
    let task = TaskId::new();
    let result_revision = "a".repeat(40);
    let mut worktree = fake_worktree(session, child, "wt-1", "repo");
    worktree.task_id = task;
    let project = fake_project(session, &[session], vec![worktree.clone()]);
    let mut integrated = worktree.clone();
    integrated.status = ProjectWorktreeStatus::Integrated;
    integrated.result_revision = Some(result_revision.clone());
    integrated.integrated_revision = Some(result_revision.clone());
    let session_text = session.to_string();
    let task_text = task.to_string();

    // Without the flag the request expects the revision the checkout and the
    // review were created from.
    let sent = Rc::new(RefCell::new(Vec::new()));
    let mut client = worktree_client(
        &fixture.backend,
        session,
        project.clone(),
        ProjectAnswer::Updated(Box::new(integrated.clone())),
        Rc::clone(&sent),
    );
    let outcome = run(
        &mut client,
        &[
            "project",
            "worktree",
            "integrate",
            &session_text,
            "--task",
            &task_text,
        ],
    );
    assert_eq!(outcome.code, exit::OK, "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        format!(
            "integrated worktree wt-1 repo\n\
             child-revision {result_revision}\n\
             integrated-revision {result_revision}\n"
        )
    );
    assert_eq!(
        integrate_requests(&sent),
        vec![(
            worktree.project_id,
            session,
            task,
            worktree.base_revision.clone(),
        )]
    );
    drop(client);

    // The flag replaces the record's base revision, and the payload reports the
    // revision it was accepted against.
    let expected = "d".repeat(40);
    let flagged_sent = Rc::new(RefCell::new(Vec::new()));
    let mut flagged = worktree_client(
        &fixture.backend,
        session,
        project.clone(),
        ProjectAnswer::Updated(Box::new(integrated.clone())),
        Rc::clone(&flagged_sent),
    );
    let payload = run(
        &mut flagged,
        &[
            "project",
            "worktree",
            "integrate",
            &session_text,
            "--task",
            &task_text,
            "--expected-parent-revision",
            &expected,
            "--json",
        ],
    );
    assert_eq!(payload.code, exit::OK, "{}", payload.stderr);
    let envelope = json_envelope(&payload);
    assert_eq!(envelope["verb"], "project worktree integrate");
    assert_eq!(envelope["data"]["expected_parent_revision"], expected);
    assert_eq!(envelope["data"]["worktree"]["status"], "integrated");
    assert_eq!(
        integrate_requests(&flagged_sent),
        vec![(worktree.project_id, session, task, expected.clone())]
    );
    drop(flagged);

    // A conflict is reported as the failure it is, after exactly one request.
    let conflict_sent = Rc::new(RefCell::new(Vec::new()));
    let mut conflict = worktree_client(
        &fixture.backend,
        session,
        project.clone(),
        ProjectAnswer::Refused(loom_core::LoomError::new(
            loom_core::ErrorCode::Conflict,
            "the parent moved past the expected revision; conflicting paths: src/lib.rs",
            false,
        )),
        Rc::clone(&conflict_sent),
    );
    let refused = run(
        &mut conflict,
        &[
            "project",
            "worktree",
            "integrate",
            &session_text,
            "--task",
            &task_text,
        ],
    );
    assert_eq!(refused.code, exit::INTERNAL, "{}", refused.stderr);
    assert!(refused.stderr.contains("conflict"), "{}", refused.stderr);
    assert!(
        refused.stderr.contains("conflicting paths: src/lib.rs"),
        "{}",
        refused.stderr
    );
    assert_eq!(
        integrate_requests(&conflict_sent).len(),
        1,
        "a conflict is never retried"
    );
    drop(conflict);

    // A task without a record is refused before the integration request.
    let missing_sent = Rc::new(RefCell::new(Vec::new()));
    let mut missing = worktree_client(
        &fixture.backend,
        session,
        fake_project(session, &[session], Vec::new()),
        ProjectAnswer::Updated(Box::new(integrated.clone())),
        Rc::clone(&missing_sent),
    );
    let refused = run(
        &mut missing,
        &[
            "project",
            "worktree",
            "integrate",
            &session_text,
            "--task",
            &task_text,
        ],
    );
    assert_eq!(refused.code, exit::USAGE, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("no project worktree is recorded"),
        "{}",
        refused.stderr
    );
    assert!(integrate_requests(&missing_sent).is_empty());
    drop(missing);
    close_persistent_fixture(fixture, directory);
}
