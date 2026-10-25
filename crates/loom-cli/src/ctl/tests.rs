//! Tests for the `loom-ctl` contract, driven through the in-process backend.
//!
//! Every test talks to a real `InProcessBackend` over the same `Client` seam the
//! binary uses, so the pinned output, exit codes, and cursor behaviour are the
//! behaviour of the shipped code rather than of a test double. The only test
//! doubles rewrite a protocol answer that would otherwise need a pruned journal
//! or a second worker.

use std::{path::PathBuf, sync::Arc, time::Duration};

use loom_core::{
    AgentSessionId, AgentSessionSnapshot, Capability, CapabilitySet, EventSequence,
    ProtocolVersion, RunId, ToolCallId, WorkspaceId,
};
use loom_model::ModelId;
use loom_protocol::{
    ActivityDetail, AgentEvent, CURRENT_PROTOCOL_VERSION, ClientRequest, ControlRequest,
    ControlResponse, EventsRequest, EventsResponse, RequestEnvelope, ResponseEnvelope, RunRequest,
    RunResponse, ServerEvent, ServerEventEnvelope, ServerResponse, SessionRequest, SessionResponse,
    WorkspaceRequest, WorkspaceResponse,
};
use loom_server::{InProcessBackend, InProcessConnection};

use crate::ctl::args::{self, Format, Parsed, Verb};
use crate::ctl::client::Client;
use crate::ctl::client::testing::{InProcessClient, Wrapped};
use crate::ctl::exit;
use crate::ctl::run_command;
use crate::ctl::text;

/// The connection URL the cursor-file tests pretend to have connected to.
const URL: &str = "ws://ctl.test.invalid/control";

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
    std::env::temp_dir().join(format!("loom-ctl-{}-{suffix}", WorkspaceId::new()))
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
            "protocol 11.11\ncapabilities 8 read_agent_session,subscribe_session_events,\
             start_agent_run,read_worker_node_status,json_protocol,manage_workspaces,\
             manage_session_repositories,write_session_filesystem\nparticipant operator\n\
             node {} NODE\n",
            node.name
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

// 5. The `--json` envelopes and the exit-code classes.

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

// 7. The parser.

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
fn help_short_circuits_parsing_whatever_else_is_given() {
    assert!(matches!(parse(&["--help"]), Ok(Parsed::Help)));
    assert!(matches!(parse(&["-h"]), Ok(Parsed::Help)));
    assert!(matches!(
        parse(&["events", "--help", "--mystery"]),
        Ok(Parsed::Help)
    ));
    assert!(args::HELP.contains("loom-ctl"));
    assert!(args::HELP.contains("--cursor-file"));
    assert!(args::HELP.contains("--token-file"));
}
