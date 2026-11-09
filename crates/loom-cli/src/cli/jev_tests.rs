//! Tests for the `jev` verbs, driven against a local mock HTTP server.
//!
//! No test calls the real TypeSafe or OpenCode endpoint: every decision call
//! goes to a server this module starts on `127.0.0.1` with an ephemeral port,
//! which records the request it received and answers with a fixed body. The
//! worker side of `jev triage` runs through the same in-process backend the
//! other CLI tests use, so the approval it may send is a real control request.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use loom_core::{
    AgentSessionId, ApprovalPolicy, Capability, CapabilitySet, PolicyDecision, RequestId,
    WorkspaceId,
};
use loom_model::ModelId;
use loom_protocol::{
    ActivityDetail, AgentEvent, AgentRunState, ApprovalDecision, CURRENT_PROTOCOL_VERSION,
    ClientRequest, CollaborationRequest, ControlRequest, ControlResponse, EventsRequest,
    EventsResponse, RequestEnvelope, RunRequest, RunResponse, ServerEvent, ServerResponse,
    SessionRequest, SessionResponse, WorkspaceRequest, WorkspaceResponse,
};
use loom_server::{InProcessBackend, InProcessConnection};

use crate::cli::args::{self, Parsed, Verb};
use crate::cli::client::Client;
use crate::cli::client::testing::{InProcessClient, Wrapped};
use crate::cli::exit;
use crate::cli::run_command;

/// The key the mock server expects, which no output may ever contain.
const KEY: &str = "jev-test-key-value";

/// What one mock server received.
#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    path: String,
    authorization: Option<String>,
    user_agent: Option<String>,
    body: String,
}

/// A local HTTP server that answers one fixed response and records the request.
struct MockJev {
    address: String,
    endpoint: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockJev {
    /// Starts a server that answers every request with `status` and `body`.
    fn start(status: u16, body: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port");
        let address = listener.local_addr().expect("a bound address").to_string();
        listener
            .set_nonblocking(true)
            .expect("a non-blocking listener");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let shutdown = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&shutdown);
        let body = body.to_owned();
        let handle = thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Some((stream, request)) = read_request(stream) {
                            // Record before answering: a client that already has
                            // the answer must always see the request it made.
                            recorded.lock().expect("the request log").push(request);
                            answer(stream, status, &body);
                        }
                    }
                    Err(_) => thread::sleep(Duration::from_millis(2)),
                }
            }
        });
        Self {
            endpoint: format!("http://{address}/v1/systemone"),
            address,
            requests,
            shutdown,
            handle: Some(handle),
        }
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().expect("the request log").clone()
    }
}

impl Drop for MockJev {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Wake the accept loop so the thread observes the flag and ends.
        let _ = TcpStream::connect(&self.address);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Reads one request and returns it with its stream, without answering it yet.
fn read_request(mut stream: TcpStream) -> Option<(TcpStream, Recorded)> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    let head_end = loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut words = request_line.split_whitespace();
    let method = words.next().unwrap_or_default().to_owned();
    let path = words.next().unwrap_or_default().to_owned();
    let mut authorization = None;
    let mut user_agent = None;
    let mut content_length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().to_owned();
        match name.to_ascii_lowercase().as_str() {
            "authorization" => authorization = Some(value),
            "user-agent" => user_agent = Some(value),
            "content-length" => content_length = value.parse().unwrap_or_default(),
            _ => {}
        }
    }
    while buffer.len() < head_end + content_length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = String::from_utf8_lossy(&buffer[head_end..]).into_owned();
    let recorded = Recorded {
        method,
        path,
        authorization,
        user_agent,
        body,
    };
    Some((stream, recorded))
}

/// Answers one read request with `status` and `answer`.
fn answer(mut stream: TcpStream, status: u16, answer: &str) {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\nconnection: close\r\n\
         content-length: {length}\r\n\r\n{answer}",
        reason = if status == 200 { "OK" } else { "Error" },
        length = answer.len(),
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// A backend with one workspace and one session, plus a direct connection the
/// test uses to read the state the CLI reads.
struct Fixture {
    backend: Arc<InProcessBackend>,
    control: InProcessConnection,
    session: AgentSessionId,
}

fn fixture() -> Fixture {
    let backend = InProcessBackend::new();
    let control = backend.connect();
    negotiate(&control);
    let workspace = create_workspace(&control);
    let session = create_session(&control, workspace);
    Fixture {
        backend,
        control,
        session,
    }
}

fn negotiate(connection: &InProcessConnection) {
    let capabilities = CapabilitySet::new([
        Capability::CreateAgentSession,
        Capability::ReadAgentSession,
        Capability::SubscribeSessionEvents,
        Capability::ManageWorkspaces,
        Capability::ReadSessionFilesystem,
        Capability::ReadWorkerNodeStatus,
        Capability::JsonProtocol,
        Capability::StartAgentRun,
        Capability::ReadAgentRun,
        Capability::ConfigureApprovalPolicy,
        Capability::ControlAgentSession,
        Capability::ApproveAgentAction,
        Capability::ControlAgentRun,
        Capability::ReadProject,
    ]);
    let response = connection.request(RequestEnvelope::new(ClientRequest::Control(
        ControlRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities,
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

fn create_workspace(connection: &InProcessConnection) -> WorkspaceId {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "jev test workspace".to_owned(),
            },
        )))
        .result
    {
        Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(record))) => record.id,
        other => panic!("unexpected workspace creation response: {other:?}"),
    }
}

fn create_session(connection: &InProcessConnection, workspace_id: WorkspaceId) -> AgentSessionId {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id,
                name: "triage me".to_owned(),
            },
        )))
        .result
    {
        Ok(ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot))) => snapshot.id,
        other => panic!("unexpected session creation response: {other:?}"),
    }
}

/// Pins a policy whose `read` dimension asks, so the deterministic run parks on
/// its first read tool call instead of its first write.
fn pin_read_approval_policy(connection: &InProcessConnection, session_id: AgentSessionId) {
    let policy = ApprovalPolicy {
        read: PolicyDecision::RequireApproval,
        ..ApprovalPolicy::default()
    };
    let response = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::SetSessionApprovalPolicy {
            session_id,
            policy,
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

/// Pins the shipped default policy, so the deterministic run parks on its first
/// write (`apply_patch`).
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

fn start_run(connection: &InProcessConnection, session_id: AgentSessionId) {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id,
            task: "look around the workspace".to_owned(),
            model: ModelId::new("deterministic/demo"),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    assert!(
        matches!(
            response.result,
            Ok(ServerResponse::Run(RunResponse::AgentRunStarted(_)))
        ),
        "run start: {:?}",
        response.result
    );
}

/// The session's metadata projection, as the CLI reads it.
fn projection(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
) -> loom_protocol::AgentSessionSnapshotProjection {
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

/// Polls until the run parks on an approval, and returns the call it waits on.
fn await_pending_approval(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
) -> loom_model::ToolCall {
    for _ in 0..1200 {
        if let Some(active) = projection(connection, session_id).active_run
            && active.run.state == AgentRunState::AwaitingApproval
            && let Some(call) = active.pending_approval
        {
            return call;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the run never parked on an approval");
}

/// What one CLI run printed and returned.
struct Outcome {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run<C: Client>(client: &mut C, arguments: &[&str]) -> Outcome {
    let arguments: Vec<String> = arguments.iter().map(|value| (*value).to_owned()).collect();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime");
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = runtime.block_on(run_command(client, &arguments, &mut out, &mut err));
    Outcome {
        code,
        stdout: String::from_utf8(out).expect("UTF-8 stdout"),
        stderr: String::from_utf8(err).expect("UTF-8 stderr"),
    }
}

fn envelope(outcome: &Outcome) -> serde_json::Value {
    serde_json::from_str(outcome.stdout.trim())
        .unwrap_or_else(|error| panic!("'{}' is not one JSON envelope: {error}", outcome.stdout))
}

/// Writes the key file a test's mock server expects.
fn key_file(label: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "loom-jev-{label}-{}-{}.key",
        std::process::id(),
        RequestId::new()
    ));
    std::fs::write(&path, format!("{KEY}\n")).unwrap();
    path
}

/// The answer body the mock server sends for a given choice and confidence.
fn choice_body(choice: &str, confidence: f64) -> String {
    serde_json::json!({
        "answers": {"decision": {
            "type": "choice",
            "choice": choice,
            "confidence": confidence,
            "probabilities": {"ask_user": 0.14, "approve": confidence},
        }},
        "model": "jev-test-1",
    })
    .to_string()
}

/// A client that fails every request, which proves a verb never asks a worker.
fn unreachable_client() -> Wrapped<InProcessClient> {
    Wrapped::new(InProcessClient::new(&InProcessBackend::new())).fail_on(|_| {
        Some(loom_core::LoomError::new(
            loom_core::ErrorCode::Internal,
            "this test client must not be asked anything",
            false,
        ))
    })
}

#[test]
fn jev_status_reports_the_source_key_and_endpoint_without_the_key() {
    let key = key_file("status");
    let mut client = unreachable_client();

    // No key file at all: a successful determination with the path it would read.
    let missing = run(
        &mut client,
        &[
            "jev",
            "status",
            "--source",
            "typesafe",
            "--key-file",
            "/nonexistent/jev.key",
            "--json",
        ],
    );
    assert_eq!(missing.code, exit::OK, "{}", missing.stderr);
    let value = envelope(&missing);
    assert_eq!(value["verb"], "jev status");
    assert_eq!(value["data"]["source"], "typesafe");
    assert_eq!(
        value["data"]["endpoint"],
        "https://api.typesafe.ai/v1/systemone"
    );
    assert_eq!(value["data"]["model"], "jev-latest");
    assert_eq!(value["data"]["key_file"], "/nonexistent/jev.key");
    assert_eq!(value["data"]["key_configured"], false);
    assert_eq!(
        value["data"]["user_agent"],
        concat!("loom-cli/", env!("CARGO_PKG_VERSION"))
    );

    // The OpenCode source names its own endpoint, model, and default key path.
    let opencode = run(
        &mut client,
        &["jev", "status", "--source", "opencode", "--json"],
    );
    assert_eq!(opencode.code, exit::OK, "{}", opencode.stderr);
    let value = envelope(&opencode);
    assert_eq!(value["data"]["model"], "jev-1.13-free");
    assert_eq!(
        value["data"]["endpoint"],
        "https://opencode.ai/inference/systemone/v1/systemone"
    );
    assert!(
        value["data"]["key_file"]
            .as_str()
            .unwrap_or_default()
            .ends_with("/.config/loom/opencode.key"),
        "{value}"
    );

    // A configured key is reported as present, and the key itself never appears.
    let key_text = key.to_string_lossy().into_owned();
    let configured = run(
        &mut client,
        &[
            "jev",
            "status",
            "--source",
            "typesafe",
            "--key-file",
            &key_text,
        ],
    );
    assert_eq!(configured.code, exit::OK, "{}", configured.stderr);
    assert_eq!(
        configured.stdout,
        format!(
            "source typesafe\nendpoint https://api.typesafe.ai/v1/systemone\n\
             model jev-latest\nkey configured {key_text}\n"
        )
    );
    assert!(!configured.stdout.contains(KEY));
    let _ = std::fs::remove_file(&key);
}

#[test]
fn jev_ask_sends_the_documented_request_and_prints_the_answer() {
    let server = MockJev::start(200, &choice_body("approve", 0.86));
    let key = key_file("ask");
    let key_text = key.to_string_lossy().into_owned();
    let endpoint = server.endpoint().to_owned();
    let mut client = unreachable_client();

    let outcome = run(
        &mut client,
        &[
            "jev",
            "ask",
            "--state",
            "The build passes and the diff is two lines.",
            "--question",
            "May the change be merged?",
            "--option",
            "approve=safe to merge",
            "--option",
            "ask_user=a person must look",
            "--source",
            "typesafe",
            "--key-file",
            &key_text,
            "--endpoint",
            &endpoint,
        ],
    );
    assert_eq!(outcome.code, exit::OK, "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        "choice approve 0.86\nprobability approve 0.86\nprobability ask_user 0.14\n\
         model jev-test-1\n"
    );
    assert!(!outcome.stdout.contains(KEY), "the key is never printed");

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/systemone");
    assert_eq!(
        request.authorization.as_deref(),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert_eq!(
        request.user_agent.as_deref(),
        Some(concat!("loom-cli/", env!("CARGO_PKG_VERSION")))
    );
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("a JSON request body");
    assert_eq!(body["model"], "jev-latest");
    assert_eq!(body["state"], "The build passes and the diff is two lines.");
    let question = &body["questions"]["decision"];
    assert_eq!(question["type"], "choice");
    assert_eq!(question["instructions"], "May the change be merged?");
    assert_eq!(question["criteria"]["approve"], "safe to merge");
    assert_eq!(question["criteria"]["ask_user"], "a person must look");

    // The same answer in `--json` carries the numbers, not the rendered lines.
    let json = run(
        &mut client,
        &[
            "jev",
            "ask",
            "--state",
            "state",
            "--question",
            "question",
            "--option",
            "approve",
            "--option",
            "ask_user",
            "--key-file",
            &key_text,
            "--endpoint",
            &endpoint,
            "--json",
        ],
    );
    assert_eq!(json.code, exit::OK, "{}", json.stderr);
    let value = envelope(&json);
    assert_eq!(value["verb"], "jev ask");
    assert_eq!(value["data"]["choice"], "approve");
    assert_eq!(value["data"]["confidence"], 0.86);
    assert_eq!(value["data"]["model"], "jev-test-1");
    assert_eq!(
        value["data"]["probabilities"],
        serde_json::json!([
            {"label": "approve", "probability": 0.86},
            {"label": "ask_user", "probability": 0.14},
        ])
    );
    let _ = std::fs::remove_file(&key);
}

#[test]
fn jev_ask_maps_the_service_failures_and_never_echoes_the_key() {
    let cases = [
        (
            401,
            r#"{"detail":{"error_type":"authentication_error","message":"Cannot authenticate"}}"#,
            exit::MISMATCH,
            "authentication_failed",
            "Cannot authenticate",
        ),
        (
            429,
            r#"{"error":{"type":"rate_limit","message":"slow down"}}"#,
            exit::INTERNAL,
            "provider_rate_limited",
            "rate_limit: slow down",
        ),
        (
            400,
            r#"{"detail":{"error_type":"api_usage_error","message":"Unknown model: nope"}}"#,
            exit::USAGE,
            "invalid_request",
            "Unknown model: nope",
        ),
    ];
    for (status, body, code, error_code, detail) in cases {
        let server = MockJev::start(status, body);
        let key = key_file("failure");
        let key_text = key.to_string_lossy().into_owned();
        let endpoint = server.endpoint().to_owned();
        let mut client = unreachable_client();
        let outcome = run(
            &mut client,
            &[
                "jev",
                "ask",
                "--state",
                "state",
                "--question",
                "question",
                "--option",
                "approve",
                "--option",
                "ask_user",
                "--key-file",
                &key_text,
                "--endpoint",
                &endpoint,
            ],
        );
        assert_eq!(outcome.code, code, "{status}: {}", outcome.stderr);
        assert!(
            outcome.stderr.contains(error_code),
            "{status}: {}",
            outcome.stderr
        );
        assert!(
            outcome.stderr.contains(detail),
            "{status}: {}",
            outcome.stderr
        );
        assert!(
            !outcome.stderr.contains(KEY),
            "{status}: the key is never echoed"
        );
        assert!(!outcome.stdout.contains(KEY), "{status}");
        let _ = std::fs::remove_file(&key);
    }
}

#[test]
fn jev_ask_reports_an_unreadable_key_before_any_call() {
    let server = MockJev::start(200, &choice_body("approve", 1.0));
    let endpoint = server.endpoint().to_owned();
    let mut client = unreachable_client();
    let outcome = run(
        &mut client,
        &[
            "jev",
            "ask",
            "--state",
            "state",
            "--question",
            "question",
            "--option",
            "approve",
            "--option",
            "ask_user",
            "--key-file",
            "/nonexistent/jev.key",
            "--endpoint",
            &endpoint,
        ],
    );
    assert_eq!(outcome.code, exit::USAGE, "{}", outcome.stderr);
    assert!(
        outcome.stderr.contains("/nonexistent/jev.key"),
        "{}",
        outcome.stderr
    );
    assert!(
        server.requests().is_empty(),
        "a missing key sends no request"
    );
}

/// A client that counts the control requests one verb sends.
fn counting_controls(
    backend: &Arc<InProcessBackend>,
    controls: &std::rc::Rc<std::cell::Cell<usize>>,
) -> Wrapped<InProcessClient> {
    let hook = std::rc::Rc::clone(controls);
    Wrapped::new(InProcessClient::new(backend)).before(move |envelope| {
        if matches!(
            envelope.request,
            ClientRequest::Collaboration(CollaborationRequest::ControlRun { .. })
        ) {
            hook.set(hook.get() + 1);
        }
    })
}

/// The arguments every triage test shares: the key file and the mock endpoint.
fn triage_arguments(session: AgentSessionId, key: &str, endpoint: &str) -> Vec<String> {
    [
        "jev",
        "triage",
        "--session",
        &session.to_string(),
        "--key-file",
        key,
        "--endpoint",
        endpoint,
    ]
    .map(str::to_owned)
    .to_vec()
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

/// Whether the worker recorded the approval of this call, which is the durable
/// proof that an `--apply` actually approved something.
fn approved_in_journal(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
    tool_call_id: loom_core::ToolCallId,
) -> bool {
    for _ in 0..1200 {
        let response = connection.request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id: Some(session_id),
                workspace_id: None,
                after_sequence: None,
                stream_epoch: None,
                activity_detail: ActivityDetail::Full,
            },
        )));
        if let Ok(ServerResponse::Events(EventsResponse::SessionEvents { events, .. })) =
            response.result
        {
            let approved = events.iter().any(|envelope| {
                matches!(
                    &envelope.event,
                    ServerEvent::Agent {
                        event: AgentEvent::ToolApprovalDecided {
                            tool_call_id: decided,
                            decision,
                            ..
                        }
                    } if *decided == tool_call_id && matches!(decision, ApprovalDecision::Approved)
                )
            });
            if approved {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn jev_triage_suggests_a_verdict_and_changes_nothing() {
    let fixture = fixture();
    pin_read_approval_policy(&fixture.control, fixture.session);
    start_run(&fixture.control, fixture.session);
    let call = await_pending_approval(&fixture.control, fixture.session);
    assert_eq!(call.name, "list_files", "a read tool parks the run");

    let server = MockJev::start(200, &choice_body("approve", 0.95));
    let key = key_file("triage");
    let key_text = key.to_string_lossy().into_owned();
    let endpoint = server.endpoint().to_owned();
    let controls = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let mut client = counting_controls(&fixture.backend, &controls);
    let mut arguments = triage_arguments(fixture.session, &key_text, &endpoint);
    arguments.push("--json".to_owned());

    let outcome = run_owned(&mut client, &arguments);
    assert_eq!(outcome.code, exit::OK, "{}", outcome.stderr);
    let value = envelope(&outcome);
    assert_eq!(value["verb"], "jev triage");
    assert_eq!(value["data"]["verdict"], "approve");
    assert_eq!(value["data"]["confidence"], 0.95);
    assert_eq!(value["data"]["applied"], false);
    assert_eq!(value["data"]["skipped"], serde_json::Value::Null);
    assert_eq!(value["data"]["tool_name"], "list_files");
    assert_eq!(value["data"]["tool_call"], call.id.to_string());
    assert_eq!(value["data"]["read_tool"], true);
    assert_eq!(value["data"]["paths_inside_root"], true);
    assert_eq!(value["data"]["threshold"], 0.9);
    assert_eq!(
        controls.get(),
        0,
        "a suggestion without --apply sends no control request"
    );
    assert_eq!(
        await_pending_approval(&fixture.control, fixture.session).id,
        call.id,
        "the call is still pending"
    );

    // The state the model was asked about names the tool and the workspace.
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_str(&requests[0].body).expect("a JSON body");
    let state = body["state"].as_str().unwrap_or_default();
    assert!(state.contains("list_files"), "{state}");
    assert!(state.contains("parked on a tool call"), "{state}");
    assert!(
        body["questions"]["decision"]["criteria"]["approve"]
            .as_str()
            .map(str::len)
            .unwrap_or_default()
            > 0
    );
    assert!(
        !requests[0].body.contains(KEY),
        "the key is never in a body"
    );
    let _ = std::fs::remove_file(&key);
}

#[test]
fn jev_triage_apply_approves_a_read_inside_the_session_root() {
    let fixture = fixture();
    pin_read_approval_policy(&fixture.control, fixture.session);
    start_run(&fixture.control, fixture.session);
    let call = await_pending_approval(&fixture.control, fixture.session);

    let server = MockJev::start(200, &choice_body("approve", 0.99));
    let key = key_file("triage-apply");
    let key_text = key.to_string_lossy().into_owned();
    let endpoint = server.endpoint().to_owned();
    let controls = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let mut client = counting_controls(&fixture.backend, &controls);
    let mut arguments = triage_arguments(fixture.session, &key_text, &endpoint);
    arguments.push("--apply".to_owned());

    let outcome = run_owned(&mut client, &arguments);
    assert_eq!(outcome.code, exit::OK, "{}", outcome.stderr);
    assert_eq!(
        controls.get(),
        1,
        "exactly one approved control request: {}",
        outcome.stdout
    );
    assert_eq!(outcome.stdout.lines().count(), 3, "{}", outcome.stdout);
    assert_eq!(
        outcome.stdout.lines().next().unwrap_or_default(),
        format!(
            "session {} tool list_files call {}",
            fixture.session, call.id
        )
    );
    assert_eq!(
        outcome.stdout.lines().nth(1).unwrap_or_default(),
        "verdict approve 0.99"
    );
    assert!(
        outcome
            .stdout
            .lines()
            .nth(2)
            .unwrap_or_default()
            .starts_with("apply approved "),
        "{}",
        outcome.stdout
    );

    assert!(
        approved_in_journal(&fixture.control, fixture.session, call.id),
        "the worker recorded the approval of the pending call"
    );
    assert_ne!(
        projection(&fixture.control, fixture.session)
            .active_run
            .and_then(|active| active.pending_approval)
            .map(|call| call.id),
        Some(call.id),
        "the call is no longer pending"
    );
    let _ = std::fs::remove_file(&key);
}

// PART5

#[test]
fn jev_triage_apply_refuses_a_call_that_is_not_a_read() {
    let fixture = fixture();
    pin_default_policy(&fixture.control, fixture.session);
    start_run(&fixture.control, fixture.session);
    let call = await_pending_approval(&fixture.control, fixture.session);
    assert_eq!(call.name, "apply_patch", "a write parks the default policy");

    let server = MockJev::start(200, &choice_body("approve", 1.0));
    let key = key_file("triage-write");
    let key_text = key.to_string_lossy().into_owned();
    let endpoint = server.endpoint().to_owned();
    let controls = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let mut client = counting_controls(&fixture.backend, &controls);
    let mut arguments = triage_arguments(fixture.session, &key_text, &endpoint);
    arguments.push("--apply".to_owned());
    arguments.push("--json".to_owned());

    let outcome = run_owned(&mut client, &arguments);
    assert_eq!(outcome.code, exit::OK, "{}", outcome.stderr);
    let value = envelope(&outcome);
    assert_eq!(value["data"]["verdict"], "approve");
    assert_eq!(value["data"]["read_tool"], false);
    assert_eq!(value["data"]["applied"], false);
    assert_eq!(value["data"]["skipped"], "not-a-read-tool");
    assert_eq!(controls.get(), 0, "a write is never approved by this verb");
    assert_eq!(
        await_pending_approval(&fixture.control, fixture.session).id,
        call.id
    );
    let _ = std::fs::remove_file(&key);
}

#[test]
fn jev_triage_apply_refuses_a_path_outside_the_session_root() {
    let fixture = fixture();
    pin_read_approval_policy(&fixture.control, fixture.session);
    start_run(&fixture.control, fixture.session);
    let call = await_pending_approval(&fixture.control, fixture.session);

    // Rewrite the pending call's arguments before the CLI reads them, so the run
    // really is parked on a read tool that names a path outside its root.
    let mut rewritten = projection(&fixture.control, fixture.session);
    if let Some(active) = rewritten.active_run.as_mut()
        && let Some(pending) = active.pending_approval.as_mut()
    {
        pending.arguments = serde_json::json!({"path": "../outside/secret"});
    }
    let scripted = rewritten.clone();
    let server = MockJev::start(200, &choice_body("approve", 1.0));
    let key = key_file("triage-path");
    let key_text = key.to_string_lossy().into_owned();
    let endpoint = server.endpoint().to_owned();
    let controls = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let hook = std::rc::Rc::clone(&controls);
    let mut client = Wrapped::new(InProcessClient::new(&fixture.backend))
        .after(move |request, response| {
            if matches!(
                request,
                ClientRequest::Session(SessionRequest::GetAgentSessionSnapshotMetadata { .. })
            ) && let Ok(ServerResponse::Session(SessionResponse::AgentSessionSnapshot(
                projection,
            ))) = &mut response.result
            {
                **projection = scripted.clone();
            }
        })
        .before(move |envelope| {
            if matches!(
                envelope.request,
                ClientRequest::Collaboration(CollaborationRequest::ControlRun { .. })
            ) {
                hook.set(hook.get() + 1);
            }
        });
    let mut arguments = triage_arguments(fixture.session, &key_text, &endpoint);
    arguments.push("--apply".to_owned());
    arguments.push("--json".to_owned());

    let outcome = run_owned(&mut client, &arguments);
    assert_eq!(outcome.code, exit::OK, "{}", outcome.stderr);
    let value = envelope(&outcome);
    assert_eq!(value["data"]["tool_name"], "list_files");
    assert_eq!(value["data"]["read_tool"], true);
    assert_eq!(value["data"]["paths_inside_root"], false);
    assert_eq!(value["data"]["applied"], false);
    assert_eq!(value["data"]["skipped"], "path-outside-session-root");
    assert_eq!(controls.get(), 0, "a path outside the root is not approved");
    assert_eq!(
        await_pending_approval(&fixture.control, fixture.session).id,
        call.id
    );
    let _ = std::fs::remove_file(&key);
}

#[test]
fn jev_triage_apply_refuses_confidence_below_the_threshold() {
    let fixture = fixture();
    pin_read_approval_policy(&fixture.control, fixture.session);
    start_run(&fixture.control, fixture.session);
    let call = await_pending_approval(&fixture.control, fixture.session);

    let server = MockJev::start(200, &choice_body("approve", 0.5));
    let key = key_file("triage-threshold");
    let key_text = key.to_string_lossy().into_owned();
    let endpoint = server.endpoint().to_owned();
    let controls = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let mut client = counting_controls(&fixture.backend, &controls);
    let mut arguments = triage_arguments(fixture.session, &key_text, &endpoint);
    for flag in ["--apply", "--threshold", "0.9", "--json"] {
        arguments.push(flag.to_owned());
    }

    let outcome = run_owned(&mut client, &arguments);
    assert_eq!(outcome.code, exit::OK, "{}", outcome.stderr);
    let value = envelope(&outcome);
    assert_eq!(value["data"]["verdict"], "approve");
    assert_eq!(value["data"]["applied"], false);
    assert_eq!(value["data"]["skipped"], "below-threshold");
    assert_eq!(controls.get(), 0, "a soft answer is not an approval");
    assert_eq!(
        await_pending_approval(&fixture.control, fixture.session).id,
        call.id
    );
    let _ = std::fs::remove_file(&key);
}

#[test]
fn jev_triage_refuses_a_verdict_it_cannot_read() {
    let fixture = fixture();
    pin_read_approval_policy(&fixture.control, fixture.session);
    start_run(&fixture.control, fixture.session);
    await_pending_approval(&fixture.control, fixture.session);

    let server = MockJev::start(200, &choice_body("merge", 1.0));
    let key = key_file("triage-unknown");
    let key_text = key.to_string_lossy().into_owned();
    let endpoint = server.endpoint().to_owned();
    let controls = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let mut client = counting_controls(&fixture.backend, &controls);
    let mut arguments = triage_arguments(fixture.session, &key_text, &endpoint);
    arguments.push("--apply".to_owned());

    let outcome = run_owned(&mut client, &arguments);
    assert_eq!(outcome.code, exit::INTERNAL, "{}", outcome.stderr);
    assert!(
        outcome.stderr.contains("provider_invalid_response"),
        "{}",
        outcome.stderr
    );
    assert!(outcome.stderr.contains("'merge'"), "{}", outcome.stderr);
    assert_eq!(controls.get(), 0);
    let _ = std::fs::remove_file(&key);
}

#[test]
fn jev_triage_reports_a_session_that_waits_on_nothing() {
    let fixture = fixture();
    let server = MockJev::start(200, &choice_body("approve", 1.0));
    let key = key_file("triage-idle");
    let key_text = key.to_string_lossy().into_owned();
    let endpoint = server.endpoint().to_owned();
    let mut client = InProcessClient::new(&fixture.backend);
    let arguments = triage_arguments(fixture.session, &key_text, &endpoint);

    let outcome = run_owned(&mut client, &arguments);
    assert_eq!(outcome.code, exit::GONE, "{}", outcome.stderr);
    assert!(
        outcome.stderr.contains("invalid_state"),
        "{}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("waits on no approval"),
        "{}",
        outcome.stderr
    );
    assert!(
        server.requests().is_empty(),
        "a session with no decision asks the model nothing"
    );
    let _ = std::fs::remove_file(&key);
}

// PART6

/// The parser and the help table of the `jev` group.
fn parse_arguments(arguments: &[&str]) -> Result<Parsed, String> {
    let arguments: Vec<String> = arguments.iter().map(|value| (*value).to_owned()).collect();
    args::parse(&arguments)
}

fn invocation(arguments: &[&str]) -> args::Invocation {
    match parse_arguments(arguments) {
        Ok(Parsed::Run(invocation)) => *invocation,
        Ok(Parsed::Help(_)) => panic!("{arguments:?} parsed as help"),
        Err(message) => panic!("{arguments:?} was rejected: {message}"),
    }
}

fn parse_error(arguments: &[&str]) -> String {
    match parse_arguments(arguments) {
        Ok(_) => panic!("{arguments:?} was accepted"),
        Err(message) => message,
    }
}

#[test]
fn jev_arguments_are_validated_before_anything_runs() {
    let session = "00000000-0000-4000-8000-000000000001";
    for (arguments, expected) in [
        (vec!["jev"], "jev requires"),
        (vec!["jev", "decide"], "unknown jev sub-verb"),
        (
            vec!["jev", "status", "--source", "openai"],
            "--source must be",
        ),
        (
            vec!["jev", "status", "--endpoint", "ftp://example.invalid"],
            "http or https",
        ),
        (vec!["jev", "ask", "--question", "q"], "requires --state"),
        (
            vec![
                "jev",
                "ask",
                "--state",
                "s",
                "--state-file",
                "/tmp/x",
                "--question",
                "q",
                "--option",
                "a",
                "--option",
                "b",
            ],
            "not both",
        ),
        (
            vec![
                "jev",
                "ask",
                "--state",
                "s",
                "--question",
                "q",
                "--option",
                "only-one",
            ],
            "at least two",
        ),
        (
            vec![
                "jev",
                "ask",
                "--state",
                "s",
                "--question",
                "q",
                "--option",
                "a",
                "--option",
                "a",
            ],
            "more than once",
        ),
        (vec!["jev", "triage"], "requires --session"),
        (
            vec!["jev", "triage", "--session", session, "--threshold", "1.5"],
            "between 0 and 1",
        ),
        (
            vec!["jev", "triage", "--session", session, "--threshold", "NaN"],
            "between 0 and 1",
        ),
        (
            vec!["jev", "status", "--apply"],
            "does not apply to 'jev status'",
        ),
    ] {
        let message = parse_error(&arguments);
        assert!(message.contains(expected), "{arguments:?}: {message}");
    }

    // The accepted forms carry the flags the verb uses, and the default
    // threshold is the documented 0.9.
    let status = invocation(&["jev", "status", "--source", "opencode"]);
    assert_eq!(args::verb_name(&status.verb), "jev status");
    let Verb::JevStatus(flags) = &status.verb else {
        panic!("unexpected verb {:?}", status.verb);
    };
    assert_eq!(flags.source.as_deref(), Some("opencode"));

    let ask = invocation(&[
        "jev",
        "ask",
        "--state",
        "state",
        "--question",
        "question",
        "--option",
        "approve=safe",
        "--option",
        "ask_user",
    ]);
    let Verb::JevAsk(ask) = &ask.verb else {
        panic!("unexpected verb {:?}", ask.verb);
    };
    assert_eq!(
        ask.options,
        vec![
            ("approve".to_owned(), "safe".to_owned()),
            ("ask_user".to_owned(), String::new()),
        ]
    );

    let triage = invocation(&["jev", "triage", "--session", session, "--apply"]);
    let Verb::JevTriage(triage) = &triage.verb else {
        panic!("unexpected verb {:?}", triage.verb);
    };
    assert!(triage.apply);
    assert_eq!(triage.threshold, args::DEFAULT_JEV_THRESHOLD);
    assert_eq!(triage.request_id, None);
}

#[test]
fn jev_local_verbs_negotiate_nothing_and_triage_negotiates_its_reads() {
    for arguments in [
        vec!["jev", "status"],
        vec![
            "jev",
            "ask",
            "--state",
            "s",
            "--question",
            "q",
            "--option",
            "a",
            "--option",
            "b",
        ],
    ] {
        let invocation = invocation(&arguments);
        assert!(args::is_local(&invocation.verb), "{arguments:?}");
        let capabilities = crate::cli::verbs::used_capabilities(&invocation.verb);
        assert!(
            !capabilities.contains(Capability::ReadAgentSession),
            "{arguments:?} must not negotiate a worker read"
        );
    }

    let session = "00000000-0000-4000-8000-000000000001";
    let plain = invocation(&["jev", "triage", "--session", session]);
    assert!(!args::is_local(&plain.verb));
    let capabilities = crate::cli::verbs::used_capabilities(&plain.verb);
    for capability in [
        Capability::ReadAgentSession,
        Capability::ReadAgentRun,
        Capability::ReadSessionFilesystem,
    ] {
        assert!(capabilities.contains(capability), "{capability:?}");
    }
    assert!(
        !capabilities.contains(Capability::ApproveAgentAction),
        "a triage that cannot apply never asks for the approval capability"
    );

    let applying = invocation(&["jev", "triage", "--session", session, "--apply"]);
    assert!(
        crate::cli::verbs::used_capabilities(&applying.verb)
            .contains(Capability::ApproveAgentAction)
    );
}

#[test]
fn every_jev_command_has_its_own_help_page() {
    for command in ["jev", "jev status", "jev ask", "jev triage"] {
        for flag in ["--help", "-h"] {
            let mut arguments: Vec<String> =
                command.split_whitespace().map(str::to_owned).collect();
            arguments.push(flag.to_owned());
            let page = args::help_text(&arguments);
            assert!(
                page.starts_with(&format!("Usage: loom-cli {command}")),
                "{command}: {page}"
            );
            assert_ne!(page, args::HELP, "{command}");
        }
    }
    // The global manual lists the group and says what `--apply` requires.
    for expected in [
        "jev status",
        "jev ask",
        "jev triage",
        "--apply",
        "session root",
    ] {
        assert!(
            args::HELP.contains(expected),
            "the manual names '{expected}'"
        );
    }
}
