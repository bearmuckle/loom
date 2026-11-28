//! End-to-end integration tests for the Antigravity ACP host in `loom-server`.
//!
//! When invoked with `LOOM_ACP_STUB_MODE=1`, this test binary acts as a real
//! compiled stdio JSON-RPC 2.0 ACP stub harness; otherwise it runs the full
//! `InProcessBackend` integration test suite against itself.

use std::{
    fs,
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use loom_core::{
    ANTIGRAVITY_AUTH_GEMINI_API_KEY, ANTIGRAVITY_AUTH_OAUTH_PERSONAL, ANTIGRAVITY_HARNESS_ID,
    ApprovalPolicy, Capability, CapabilitySet, ErrorCode, PolicyDecision, SessionBackendDescriptor,
    WorkspaceId,
};
use loom_model::{AgentRunState, ModelId};
use loom_protocol::{
    ActivityDetail, AgentEvent, CURRENT_PROTOCOL_VERSION, ClientRequest, ControlRequest,
    EventsRequest, EventsResponse, ProviderRequest, ProviderResponse, RequestEnvelope, RunRequest,
    RunResponse, ServerEvent, ServerResponse, SessionRequest, SessionResponse, WorkspaceRequest,
    WorkspaceResponse,
};
use loom_server::{AuthTokenStore, AuthorizationScope, InProcessBackend, InProcessConnection};
use serde_json::Value;

fn stub_command() -> String {
    let exe = std::env::current_exe().expect("test executable path");
    format!("env LOOM_ACP_STUB_MODE=1 {}", exe.display())
}

fn stub_command_with_env(vars: &[(&str, &str)]) -> String {
    let exe = std::env::current_exe().expect("test executable path");
    let mut parts = vec!["env".to_owned(), "LOOM_ACP_STUB_MODE=1".to_owned()];
    for (k, v) in vars {
        parts.push(format!("{k}={v}"));
    }
    parts.push(exe.display().to_string());
    parts.join(" ")
}

/// Creates a test backend with `enforce_ci_env = false` (so integration tests
/// run cleanly under GitHub Actions where `CI=true`) and an optional worker-side
/// ACP command override. The `CI=true` rejection path itself is tested
/// explicitly with default `enforce_ci_env = true` below.
fn new_test_backend(command: Option<String>) -> Arc<InProcessBackend> {
    let backend = InProcessBackend::new();
    backend.set_acp_ci_env_guard(false);
    backend.set_acp_command_override(command);
    backend
}

fn new_persistent_test_backend(
    db_path: &std::path::Path,
    command: Option<String>,
) -> Arc<InProcessBackend> {
    let backend = InProcessBackend::new_persistent(db_path).unwrap();
    backend.set_acp_ci_env_guard(false);
    backend.set_acp_command_override(command);
    backend
}

fn reopen_persistent_test_backend(
    db_path: &std::path::Path,
    command: Option<String>,
) -> Arc<InProcessBackend> {
    let backend = InProcessBackend::open_persistent(db_path).unwrap();
    backend.set_acp_ci_env_guard(false);
    backend.set_acp_command_override(command);
    backend
}

fn negotiate_with(connection: &InProcessConnection, caps: impl IntoIterator<Item = Capability>) {
    connection
        .request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::Negotiate {
                client_version: CURRENT_PROTOCOL_VERSION,
                capabilities: CapabilitySet::new(caps),
            },
        )))
        .result
        .expect("negotiation succeeds");
}

fn negotiate_all(connection: &InProcessConnection) {
    negotiate_with(
        connection,
        [
            Capability::JsonProtocol,
            Capability::ManageWorkspaces,
            Capability::CreateAgentSession,
            Capability::ReadAgentSession,
            Capability::ControlAgentSession,
            Capability::DeleteAgentSession,
            Capability::ForkAgentSession,
            Capability::StartAgentRun,
            Capability::ReadAgentRun,
            Capability::ControlAgentRun,
            Capability::ApproveAgentAction,
            Capability::ConfigureApprovalPolicy,
            Capability::SubscribeSessionEvents,
            Capability::ReadUsage,
            Capability::ListProviders,
            Capability::ConfigureProviders,
            Capability::AcpAgentSessions,
        ],
    );
}

fn create_workspace(connection: &InProcessConnection, name: &str) -> WorkspaceId {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: name.to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(ws)) => ws.id,
        other => panic!("unexpected workspace response: {other:?}"),
    }
}

fn create_acp_session(
    connection: &InProcessConnection,
    workspace_id: WorkspaceId,
    name: &str,
    auth_method: Option<&str>,
) -> loom_core::AgentSessionSnapshot {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAcpAgentSessionInWorkspace {
                workspace_id,
                name: name.to_owned(),
                harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                auth_method: auth_method.map(str::to_owned),
                interactive: true,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(s)) => s,
        other => panic!("unexpected session: {other:?}"),
    }
}

fn start_run(
    connection: &InProcessConnection,
    session_id: loom_core::AgentSessionId,
    task: &str,
    model: &str,
) -> loom_protocol::AgentRunSnapshot {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRun {
                session_id,
                task: task.to_owned(),
                model: ModelId::new(model),
                system_instructions: None,
                repository_instructions: None,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Run(RunResponse::AgentRunStarted(r)) => r,
        other => panic!("unexpected run: {other:?}"),
    }
}

fn run_projection(
    connection: &InProcessConnection,
    run_id: loom_core::RunId,
) -> Box<loom_protocol::AgentRunSnapshotProjection> {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::GetAgentRunSnapshot {
                run_id,
                activity_detail: ActivityDetail::Full,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Run(RunResponse::AgentRunSnapshot(proj)) => proj,
        other => panic!("unexpected projection: {other:?}"),
    }
}

fn await_run_state(
    connection: &InProcessConnection,
    run_id: loom_core::RunId,
    predicate: impl Fn(AgentRunState) -> bool,
) -> loom_protocol::AgentRunSnapshot {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = match connection
            .request(RequestEnvelope::new(ClientRequest::Run(
                RunRequest::GetAgentRun { run_id },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Run(RunResponse::AgentRun(snapshot)) => snapshot,
            other => panic!("unexpected run response: {other:?}"),
        };
        if predicate(snapshot.state) {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "run {run_id} did not reach expected state; last state = {:?}",
            snapshot.state
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn await_settled(
    connection: &InProcessConnection,
    run_id: loom_core::RunId,
) -> loom_protocol::AgentRunSnapshot {
    await_run_state(connection, run_id, |state| {
        matches!(
            state,
            AgentRunState::Completed
                | AgentRunState::Failed
                | AgentRunState::Cancelled
                | AgentRunState::AwaitingApproval
        )
    })
}

fn session_events(
    connection: &InProcessConnection,
    session_id: loom_core::AgentSessionId,
) -> Vec<ServerEvent> {
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
        .unwrap()
    {
        ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) => {
            events.into_iter().map(|env| env.event).collect()
        }
        other => panic!("unexpected events response: {other:?}"),
    }
}

fn test_capability_gate_and_authorization_refuse_unprivileged_clients() {
    let backend = new_test_backend(Some(stub_command()));
    let old_client = backend.connect();
    // Negotiate without AcpAgentSessions.
    negotiate_with(
        &old_client,
        [
            Capability::JsonProtocol,
            Capability::ManageWorkspaces,
            Capability::CreateAgentSession,
            Capability::StartAgentRun,
            Capability::ListProviders,
            Capability::ConfigureProviders,
        ],
    );
    let workspace_id = create_workspace(&old_client, "Compat");

    let err = old_client
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAcpAgentSessionInWorkspace {
                workspace_id,
                name: "ACP".to_owned(),
                harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                auth_method: None,
                interactive: true,
            },
        )))
        .result
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::CapabilityDenied);

    let err = old_client
        .request(RequestEnvelope::new(ClientRequest::Provider(
            ProviderRequest::ProbeAcpHarness {
                harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                auth_method: None,
            },
        )))
        .result
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::CapabilityDenied);

    // R2: AcpAgentSessions alone is insufficient without CreateAgentSession / ConfigureProviders.
    let acp_only = backend.connect();
    negotiate_with(
        &acp_only,
        [Capability::JsonProtocol, Capability::AcpAgentSessions],
    );
    let err = acp_only
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAcpAgentSessionInWorkspace {
                workspace_id,
                name: "NoCreateCap".to_owned(),
                harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                auth_method: None,
                interactive: true,
            },
        )))
        .result
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::CapabilityDenied);

    let err = acp_only
        .request(RequestEnvelope::new(ClientRequest::Provider(
            ProviderRequest::ProbeAcpHarness {
                harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                auth_method: None,
            },
        )))
        .result
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::CapabilityDenied);

    // Full client creates an ACP session; older client still cannot start a run in it.
    let new_client = backend.connect();
    negotiate_all(&new_client);
    let acp_session = create_acp_session(&new_client, workspace_id, "ACP", None);

    let err = old_client
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRun {
                session_id: acp_session.id,
                task: "hello".to_owned(),
                model: ModelId::new("default"),
                system_instructions: None,
                repository_instructions: None,
            },
        )))
        .result
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::CapabilityDenied);

    // R2: Session-scoped token cannot probe the worker's ACP harness.
    let tokens = Arc::new(AuthTokenStore::new());
    let issued = tokens
        .issue(AuthorizationScope::for_sessions(
            [acp_session.id],
            CapabilitySet::new([
                Capability::JsonProtocol,
                Capability::AcpAgentSessions,
                Capability::ConfigureProviders,
            ]),
        ))
        .unwrap();
    let scoped = backend.connect_authenticated(tokens.authenticate(&issued.token).unwrap());
    negotiate_with(
        &scoped,
        [
            Capability::JsonProtocol,
            Capability::AcpAgentSessions,
            Capability::ConfigureProviders,
        ],
    );
    let err = scoped
        .request(RequestEnvelope::new(ClientRequest::Provider(
            ProviderRequest::ProbeAcpHarness {
                harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                auth_method: None,
            },
        )))
        .result
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::AuthorizationDenied);

    // R2: Participant-bound connection (non-operator) cannot probe the worker's ACP harness.
    let participant = loom_core::ParticipantIdentity {
        id: loom_core::ParticipantId::new(),
        display_name: "Alice".to_owned(),
    };
    let part_issued = tokens
        .issue_for_participant(AuthorizationScope::all(), participant)
        .unwrap();
    let participant_conn =
        backend.connect_authenticated(tokens.authenticate(&part_issued.token).unwrap());
    negotiate_all(&participant_conn);
    let err = participant_conn
        .request(RequestEnvelope::new(ClientRequest::Provider(
            ProviderRequest::ProbeAcpHarness {
                harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                auth_method: None,
            },
        )))
        .result
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::AuthorizationDenied);
}

fn test_probe_discovers_catalog_and_sanitizes_command_display() {
    let cmd = stub_command_with_env(&[("INLINE_MARKER_SECRET", "must_never_leak")]);
    let backend = new_test_backend(Some(cmd));
    let conn = backend.connect();
    negotiate_all(&conn);

    let probe = match conn
        .request(RequestEnvelope::new(ClientRequest::Provider(
            ProviderRequest::ProbeAcpHarness {
                harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                auth_method: Some(ANTIGRAVITY_AUTH_GEMINI_API_KEY.to_owned()),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Provider(ProviderResponse::AcpHarnessProbe(probe)) => probe,
        other => panic!("unexpected probe response: {other:?}"),
    };

    assert!(probe.available);
    assert_eq!(probe.protocol_version, Some(1));
    assert_eq!(probe.agent_name.as_deref(), Some("antigravity-stub"));
    assert!(probe.interactive_only);
    assert_eq!(probe.models.len(), 2);
    assert_eq!(probe.models[0].id, "gemini-catalog-pro");
    // R2: discovered_command must never leak raw command arguments or inline env assignments.
    let discovered = probe.discovered_command.unwrap_or_default();
    assert!(!discovered.contains("INLINE_MARKER_SECRET"), "{discovered}");
    assert!(!discovered.contains("must_never_leak"), "{discovered}");

    let notices = probe.notices.join("\n").to_ascii_lowercase();
    assert!(notices.contains("terms"));
    assert!(notices.contains("interactions"));
    assert!(notices.contains("not a google-registered acp client"));
}

fn test_interactive_only_guard_refuses_headless_partial_auto_approve_and_ci() {
    let backend = new_test_backend(Some(stub_command()));
    let conn = backend.connect();
    negotiate_all(&conn);
    let workspace_id = create_workspace(&conn, "Policy");

    // interactive: false is refused immediately.
    let err = conn
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAcpAgentSessionInWorkspace {
                workspace_id,
                name: "Headless".to_owned(),
                harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                auth_method: None,
                interactive: false,
            },
        )))
        .result
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRequest);
    assert!(err.message.contains("interactive-only"));

    // Interactive session starts with auto_approve_actions = false and refuses enabling auto-approve.
    let session = create_acp_session(&conn, workspace_id, "Interactive", None);

    let err = conn
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::SetSessionApprovalPolicy {
                session_id: session.id,
                policy: ApprovalPolicy::default(),
                auto_approve_actions: Some(true),
            },
        )))
        .result
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRequest);
    assert!(err.message.contains("auto-approve"));

    let err = conn
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::SetSessionApprovalPolicy {
                session_id: session.id,
                policy: ApprovalPolicy::auto_approve(),
                auto_approve_actions: Some(false),
            },
        )))
        .result
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRequest);

    // R5: A custom partial auto-approve policy (`!= ApprovalPolicy::auto_approve()`)
    // that sets write/command/network to Allow must also be refused.
    let partial_auto = ApprovalPolicy {
        read: PolicyDecision::RequireApproval,
        write: PolicyDecision::Allow,
        command: PolicyDecision::Allow,
        network: PolicyDecision::Allow,
        destructive: PolicyDecision::Allow,
    };
    assert_ne!(partial_auto, ApprovalPolicy::auto_approve());
    let err = conn
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::SetSessionApprovalPolicy {
                session_id: session.id,
                policy: partial_auto,
                auto_approve_actions: Some(false),
            },
        )))
        .result
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRequest);

    // R1 & R5: When CI/GITHUB_ACTIONS is set and default enforce_ci_env=true is active,
    // ProbeAcpHarness and CreateAcpAgentSessionInWorkspace must be refused.
    if std::env::var("CI").is_ok() || std::env::var("GITHUB_ACTIONS").is_ok() {
        let strict_backend = InProcessBackend::new();
        strict_backend.set_acp_command_override(Some(stub_command()));
        let strict_conn = strict_backend.connect();
        negotiate_all(&strict_conn);
        let strict_ws = create_workspace(&strict_conn, "CI-Strict");
        let err = strict_conn
            .request(RequestEnvelope::new(ClientRequest::Provider(
                ProviderRequest::ProbeAcpHarness {
                    harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                    auth_method: None,
                },
            )))
            .result
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
        assert!(err.message.contains("interactive-only"));

        let err = strict_conn
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateAcpAgentSessionInWorkspace {
                    workspace_id: strict_ws,
                    name: "CI-Session".to_owned(),
                    harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                    auth_method: None,
                    interactive: true,
                },
            )))
            .result
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
        assert!(err.message.contains("interactive-only"));
    }
}

fn test_full_acp_turn_multi_turn_model_switch_fork_profile_reuse_and_delete() {
    let dir = std::env::temp_dir().join(format!(
        "loom-server-acp-full-{}",
        loom_core::RequestId::new()
    ));
    fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("state.db");
    let record_path = dir.join("record.jsonl");
    let cmd = stub_command_with_env(&[
        ("LOOM_ACP_STUB_RECORD", record_path.to_str().unwrap()),
        ("LOOM_ACP_STUB_SCENARIO", "stderr"),
    ]);

    let backend = new_persistent_test_backend(&db_path, Some(cmd));
    let conn = backend.connect();
    negotiate_all(&conn);
    let workspace_id = create_workspace(&conn, "Antigravity");

    let session = create_acp_session(&conn, workspace_id, "AGY Session", None);
    assert!(session.is_antigravity());
    assert!(!session.supports_durable_resume());

    let run = match conn
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRun {
                session_id: session.id,
                task: "Explain README".to_owned(),
                model: ModelId::new("gemini-catalog-pro"),
                system_instructions: Some("SHOULD NOT BE INJECTED".to_owned()),
                repository_instructions: Some("ALSO NOT INJECTED".to_owned()),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Run(RunResponse::AgentRunStarted(r)) => r,
        other => panic!("unexpected run: {other:?}"),
    };

    let settled = await_settled(&conn, run.id);
    assert_eq!(settled.state, AgentRunState::Completed);

    // Inspect projection: plan, usage, tool activity, stderr activity, assistant reply, reasoning.
    let projection = run_projection(&conn, run.id);
    assert_eq!(projection.plan.len(), 2);
    assert_eq!(projection.usage.input_tokens, 42);
    assert_eq!(projection.usage.output_tokens, 17);
    assert!(
        projection
            .messages
            .iter()
            .any(|m| m.content.contains("Echo: Explain README")),
        "messages: {:?}",
        projection.messages
    );
    assert!(
        projection
            .activities
            .iter()
            .any(|a| matches!(&a.data, loom_protocol::AgentActivityData::File { .. })),
        "tool call activity missing: {:?}",
        projection.activities
    );
    assert!(
        projection.activities.iter().any(|a| {
            matches!(
                &a.data,
                loom_protocol::AgentActivityData::ToolCall { call, result: Some(res) }
                    if call.name == "acp_stderr" && res.output.contains("harness diagnostic notice")
            )
        }),
        "stderr activity missing: {:?}",
        projection.activities
    );

    // Verify events included ReasoningDelta.
    let events = session_events(&conn, session.id);
    assert!(events.iter().any(|e| matches!(
        e,
        ServerEvent::Agent {
            event: AgentEvent::ReasoningDelta { text, .. }
        } if text.contains("thinking through request")
    )));

    // R8: SetAgentRunModel works on ACP runs without failing on ProviderRegistry lookup.
    let updated_run = match conn
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::SetAgentRunModel {
                run_id: run.id,
                model: ModelId::new("gemini-catalog-flash"),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Run(RunResponse::AgentRun(r)) => r,
        other => panic!("unexpected set model response: {other:?}"),
    };
    assert_eq!(updated_run.model.as_str(), "gemini-catalog-flash");

    // R10: Multi-turn follow-up via SendAgentMessage on the same live ACP run.
    conn.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::SendAgentMessage {
            run_id: run.id,
            attempt_id: updated_run.attempt_id,
            expected_control_revision: updated_run.control_revision,
            message: "Follow-up question".to_owned(),
        },
    )))
    .result
    .unwrap();
    let settled2 = await_run_state(&conn, run.id, |s| s == AgentRunState::Completed);
    assert_eq!(settled2.state, AgentRunState::Completed);
    let proj2 = run_projection(&conn, run.id);
    assert!(
        proj2
            .messages
            .iter()
            .any(|m| m.content.contains("Echo: Follow-up question")),
        "follow-up message missing: {:?}",
        proj2.messages
    );

    // R3: Calling StartSessionAgentRun a second time on a completed ACP session succeeds.
    let run2 = start_run(&conn, session.id, "Second run prompt", "default");
    let settled_run2 = await_settled(&conn, run2.id);
    assert_eq!(settled_run2.state, AgentRunState::Completed);
    let proj_run2 = run_projection(&conn, run2.id);
    assert!(
        proj_run2
            .messages
            .iter()
            .any(|m| m.content.contains("Echo: Second run prompt"))
    );

    // Verify session record updated with protocol version, capabilities, remote_session_id, models,
    // and sanitized command label (never raw env/args).
    let updated_session = match conn
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::GetAgentSession {
                session_id: session.id,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSession(s)) => s,
        other => panic!("unexpected session: {other:?}"),
    };
    match updated_session.backend.as_ref().unwrap() {
        SessionBackendDescriptor::Acp(acp) => {
            assert_eq!(acp.harness, ANTIGRAVITY_HARNESS_ID);
            assert!(!acp.command.contains("LOOM_ACP_STUB_RECORD"));
            assert_eq!(acp.protocol_version, Some(1));
            assert_eq!(
                acp.auth_method.as_deref(),
                Some(ANTIGRAVITY_AUTH_OAUTH_PERSONAL)
            );
            assert!(!acp.capabilities.supports_resume());
            assert_eq!(acp.remote_session_id.as_deref(), Some("acp-stub-sess-1"));
            assert_eq!(acp.available_models.len(), 2);
        }
        other => panic!("unexpected backend: {other:?}"),
    }

    // R4: Forking an ACP session clears remote_session_id.
    let forked = match conn
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::ForkAgentSession {
                session_id: session.id,
                name: "AGY Fork".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionForked(s)) => s,
        other => panic!("unexpected fork response: {other:?}"),
    };
    let forked_acp = forked.backend.as_ref().unwrap().as_acp().unwrap();
    assert_eq!(forked_acp.remote_session_id, None);

    // R4: A second ACP session reuses the same worker-scoped isolated profile directory.
    let session2 = create_acp_session(&conn, workspace_id, "AGY Session 2", None);
    let run_s2 = start_run(&conn, session2.id, "Check profile reuse", "default");
    assert_eq!(
        await_settled(&conn, run_s2.id).state,
        AgentRunState::Completed
    );

    // Verify recorded JSON-RPC wire traffic: Loom's own clientInfo, no prompt injection,
    // and both sessions used the SAME worker-scoped isolated profile directory.
    let recorded = fs::read_to_string(&record_path).unwrap();
    assert!(recorded.contains("\"name\":\"loom\""));
    assert!(!recorded.to_ascii_lowercase().contains("t3code"));
    assert!(!recorded.contains("SHOULD NOT BE INJECTED"));
    assert!(!recorded.contains("ALSO NOT INJECTED"));
    assert!(!recorded.contains("General agent instructions"));
    let profile_dirs: Vec<String> = recorded
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| {
            v.get("profile_dir")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    assert!(
        profile_dirs.len() >= 2,
        "expected 2 spawns: {profile_dirs:?}"
    );
    assert_eq!(profile_dirs[0], profile_dirs[1]);
    assert!(PathBuf::from(&profile_dirs[0]).is_dir());

    // R4: Deleting an archived ACP session terminates its live subprocess.
    assert!(backend.has_live_acp_process(session2.id));
    conn.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::ArchiveAgentSession {
            session_id: session2.id,
        },
    )))
    .result
    .unwrap();
    conn.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::DeleteAgentSession {
            session_id: session2.id,
            force: false,
        },
    )))
    .result
    .unwrap();
    assert!(!backend.has_live_acp_process(session2.id));

    // Verify SQLite persistence round-trip preserves session backend descriptor.
    drop(conn);
    backend.shutdown().unwrap();
    let reopened = reopen_persistent_test_backend(&db_path, None);
    let conn2 = reopened.connect();
    negotiate_all(&conn2);
    let persisted_session = match conn2
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::GetAgentSession {
                session_id: session.id,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSession(s)) => s,
        other => panic!("unexpected reopened session: {other:?}"),
    };
    assert_eq!(persisted_session.backend, updated_session.backend);

    drop(conn2);
    reopened.shutdown().unwrap();
    let _ = fs::remove_dir_all(&dir);
}

fn test_server_restart_rehydrates_acp_runs_with_load_session_recap_and_pending_approval() {
    // (a) With loadSession advertised: server restart rehydrates ACP run and calls session/load.
    {
        let dir = std::env::temp_dir().join(format!(
            "loom-server-acp-restart-load-{}",
            loom_core::RequestId::new()
        ));
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("state.db");
        let record_path = dir.join("record.jsonl");
        let cmd = stub_command_with_env(&[
            ("LOOM_ACP_STUB_RECORD", record_path.to_str().unwrap()),
            ("LOOM_ACP_STUB_LOAD_SESSION", "1"),
        ]);

        let backend = new_persistent_test_backend(&db_path, Some(cmd.clone()));
        let conn = backend.connect();
        negotiate_all(&conn);
        let ws = create_workspace(&conn, "RestartLoad");
        let session = create_acp_session(&conn, ws, "LoadSess", None);
        let run = start_run(&conn, session.id, "First turn", "default");
        let done = await_settled(&conn, run.id);
        assert_eq!(done.state, AgentRunState::Completed);
        drop(conn);
        backend.shutdown().unwrap();

        let reopened = reopen_persistent_test_backend(&db_path, Some(cmd));
        let conn2 = reopened.connect();
        negotiate_all(&conn2);
        conn2
            .request(RequestEnvelope::new(ClientRequest::Run(
                RunRequest::SendAgentMessage {
                    run_id: run.id,
                    attempt_id: done.attempt_id,
                    expected_control_revision: done.control_revision,
                    message: "After restart with loadSession".to_owned(),
                },
            )))
            .result
            .unwrap();
        let done2 = await_run_state(&conn2, run.id, |s| s == AgentRunState::Completed);
        assert_eq!(done2.state, AgentRunState::Completed);
        let proj = run_projection(&conn2, run.id);
        assert!(
            proj.messages
                .iter()
                .any(|m| m.content.contains("Echo: After restart with loadSession"))
        );
        // Should NOT have injected a recap because native session/load succeeded.
        assert!(
            !proj
                .messages
                .iter()
                .any(|m| m.content.contains("Loom local transcript recap"))
        );
        let recorded = fs::read_to_string(&record_path).unwrap();
        assert!(
            recorded.contains("\"method\":\"session/load\""),
            "expected session/load after restart: {recorded}"
        );
        drop(conn2);
        reopened.shutdown().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    // (b) Without loadSession: server restart rehydrates ACP run and sends a labelled local recap.
    {
        let dir = std::env::temp_dir().join(format!(
            "loom-server-acp-restart-recap-{}",
            loom_core::RequestId::new()
        ));
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("state.db");
        let cmd = stub_command();

        let backend = new_persistent_test_backend(&db_path, Some(cmd.clone()));
        let conn = backend.connect();
        negotiate_all(&conn);
        let ws = create_workspace(&conn, "RestartRecap");
        let session = create_acp_session(&conn, ws, "RecapSess", None);
        let run = start_run(&conn, session.id, "Remember alpha-42", "default");
        let done = await_settled(&conn, run.id);
        assert_eq!(done.state, AgentRunState::Completed);
        drop(conn);
        backend.shutdown().unwrap();

        let reopened = reopen_persistent_test_backend(&db_path, Some(cmd));
        let conn2 = reopened.connect();
        negotiate_all(&conn2);
        conn2
            .request(RequestEnvelope::new(ClientRequest::Run(
                RunRequest::SendAgentMessage {
                    run_id: run.id,
                    attempt_id: done.attempt_id,
                    expected_control_revision: done.control_revision,
                    message: "What was the code?".to_owned(),
                },
            )))
            .result
            .unwrap();
        let done2 = await_run_state(&conn2, run.id, |s| s == AgentRunState::Completed);
        assert_eq!(done2.state, AgentRunState::Completed);
        let proj = run_projection(&conn2, run.id);
        // Stub echoes the full prompt sent to session/prompt, verifying the labelled recap.
        assert!(
            proj.messages.iter().any(|m| {
                m.content.contains("Loom local transcript recap")
                    && m.content.contains("Remember alpha-42")
                    && m.content.contains("What was the code?")
            }),
            "expected labelled recap in echoed prompt: {:?}",
            proj.messages
        );
        drop(conn2);
        reopened.shutdown().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    // (c) Restart while AwaitingApproval recovers to Paused with pending_approval cleared.
    {
        let dir = std::env::temp_dir().join(format!(
            "loom-server-acp-restart-perm-{}",
            loom_core::RequestId::new()
        ));
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("state.db");
        let perm_cmd = stub_command_with_env(&[("LOOM_ACP_STUB_SCENARIO", "permission")]);

        let backend = new_persistent_test_backend(&db_path, Some(perm_cmd));
        let conn = backend.connect();
        negotiate_all(&conn);
        let ws = create_workspace(&conn, "RestartPerm");
        let session = create_acp_session(&conn, ws, "PermRestart", None);
        let run = start_run(&conn, session.id, "Need approval", "default");
        let waiting = await_settled(&conn, run.id);
        assert_eq!(waiting.state, AgentRunState::AwaitingApproval);

        // Snapshot the SQLite files while the run is durably checkpointed in
        // AwaitingApproval, shut down the original backend cleanly, then restore
        // the pre-shutdown snapshot over `db_path` to simulate an abrupt crash.
        let crash_dir = dir.join("after-crash");
        fs::create_dir_all(&crash_dir).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let src = dir.join(format!("state.db{suffix}"));
            if src.exists() {
                fs::copy(&src, crash_dir.join(format!("state.db{suffix}"))).unwrap();
            }
        }
        drop(conn);
        backend.shutdown().unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let dst = dir.join(format!("state.db{suffix}"));
            let snap = crash_dir.join(format!("state.db{suffix}"));
            if snap.exists() {
                fs::copy(&snap, &dst).unwrap();
            } else {
                let _ = fs::remove_file(&dst);
            }
        }

        let reopened = reopen_persistent_test_backend(&db_path, Some(stub_command()));
        let conn2 = reopened.connect();
        negotiate_all(&conn2);
        let proj = run_projection(&conn2, run.id);
        assert_eq!(proj.run.state, AgentRunState::Paused);
        assert!(
            proj.pending_approval.is_none(),
            "stale pending approval must be cleared on restart"
        );
        // Operator can cleanly send a follow-up message after recovery.
        conn2
            .request(RequestEnvelope::new(ClientRequest::Run(
                RunRequest::SendAgentMessage {
                    run_id: run.id,
                    attempt_id: proj.run.attempt_id,
                    expected_control_revision: proj.run.control_revision,
                    message: "Continue after crash".to_owned(),
                },
            )))
            .result
            .unwrap();
        let done = await_run_state(&conn2, run.id, |s| s == AgentRunState::Completed);
        assert_eq!(done.state, AgentRunState::Completed);
        drop(conn2);
        reopened.shutdown().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}

fn test_permission_allow_deny_cancel_and_timeout_through_approval_ui() {
    for mode in ["allow", "deny", "cancel", "timeout"] {
        let cmd = stub_command_with_env(&[("LOOM_ACP_STUB_SCENARIO", "permission")]);
        let backend = new_test_backend(Some(cmd));
        if mode == "timeout" {
            backend.set_acp_timeouts(Duration::from_millis(300), Duration::from_secs(30));
        }
        let conn = backend.connect();
        negotiate_all(&conn);
        let workspace_id = create_workspace(&conn, &format!("Perm-{mode}"));
        let session = create_acp_session(
            &conn,
            workspace_id,
            &format!("Perm-{mode}"),
            Some(ANTIGRAVITY_AUTH_GEMINI_API_KEY),
        );

        let run = start_run(&conn, session.id, "Run cargo test", "default");
        let waiting = await_settled(&conn, run.id);
        assert_eq!(
            waiting.state,
            AgentRunState::AwaitingApproval,
            "mode={mode}"
        );

        let proj = run_projection(&conn, run.id);
        let pending = proj.pending_approval.expect("pending approval present");
        assert_eq!(pending.name, "acp_permission");

        match mode {
            "allow" => {
                conn.request(RequestEnvelope::new(ClientRequest::Run(
                    RunRequest::ApproveAgentAction {
                        run_id: run.id,
                        attempt_id: waiting.attempt_id,
                        expected_control_revision: waiting.control_revision,
                        tool_call_id: pending.id,
                    },
                )))
                .result
                .unwrap();
                let done = await_run_state(&conn, run.id, |s| s == AgentRunState::Completed);
                assert_eq!(done.state, AgentRunState::Completed);
                let final_proj = run_projection(&conn, run.id);
                assert!(
                    final_proj
                        .messages
                        .iter()
                        .any(|m| m.content.contains("command allowed and executed"))
                );
            }
            "deny" => {
                conn.request(RequestEnvelope::new(ClientRequest::Run(
                    RunRequest::RejectAgentAction {
                        run_id: run.id,
                        attempt_id: waiting.attempt_id,
                        expected_control_revision: waiting.control_revision,
                        tool_call_id: pending.id,
                        reason: Some("no thanks".to_owned()),
                    },
                )))
                .result
                .unwrap();
                let done = await_run_state(&conn, run.id, |s| s == AgentRunState::Completed);
                assert_eq!(done.state, AgentRunState::Completed);
                let final_proj = run_projection(&conn, run.id);
                assert!(
                    final_proj
                        .messages
                        .iter()
                        .any(|m| m.content.contains("command rejected by operator"))
                );
            }
            "cancel" => {
                conn.request(RequestEnvelope::new(ClientRequest::Run(
                    RunRequest::InterruptAgentRun { run_id: run.id },
                )))
                .result
                .unwrap();
                let done = await_run_state(&conn, run.id, |s| s == AgentRunState::Cancelled);
                assert_eq!(done.state, AgentRunState::Cancelled);
            }
            "timeout" => {
                // R6: Unanswered permission prompt times out and rejects instead of blocking forever.
                let done = await_run_state(&conn, run.id, |s| s == AgentRunState::Completed);
                assert_eq!(done.state, AgentRunState::Completed);
                let final_proj = run_projection(&conn, run.id);
                assert!(
                    final_proj
                        .messages
                        .iter()
                        .any(|m| m.content.contains("command rejected by operator"))
                );
            }
            _ => unreachable!(),
        }
        backend.shutdown().unwrap();
    }
}

fn test_cancel_wedged_timeout_crash_auth_missing_redaction_and_shutdown() {
    // 1. Cancel mid-turn
    {
        let cmd = stub_command_with_env(&[("LOOM_ACP_STUB_SCENARIO", "cancel")]);
        let backend = new_test_backend(Some(cmd));
        let conn = backend.connect();
        negotiate_all(&conn);
        let workspace_id = create_workspace(&conn, "Cancel");
        let session = create_acp_session(&conn, workspace_id, "Cancel", None);
        let run = start_run(&conn, session.id, "Long task", "default");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if session_events(&conn, session.id).iter().any(|e| {
                matches!(
                    e,
                    ServerEvent::Agent {
                        event: AgentEvent::AssistantMessageDelta { .. }
                    }
                )
            }) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        conn.request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::InterruptAgentRun { run_id: run.id },
        )))
        .result
        .unwrap();
        let settled = await_settled(&conn, run.id);
        assert_eq!(settled.state, AgentRunState::Cancelled);
        // R7: shutdown() kills remaining idle ACP child processes cleanly.
        assert!(backend.has_live_acp_process(session.id));
        backend.shutdown().unwrap();
        assert!(!backend.has_live_acp_process(session.id));
    }

    // 2. R7: Wedged harness ignoring session/cancel is killed after CANCEL_GRACE.
    {
        let cmd = stub_command_with_env(&[("LOOM_ACP_STUB_SCENARIO", "hang")]);
        let backend = new_test_backend(Some(cmd));
        let conn = backend.connect();
        negotiate_all(&conn);
        let workspace_id = create_workspace(&conn, "Wedged");
        let session = create_acp_session(&conn, workspace_id, "Wedged", None);
        let run = start_run(&conn, session.id, "Hang forever", "default");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if session_events(&conn, session.id).iter().any(|e| {
                matches!(
                    e,
                    ServerEvent::Agent {
                        event: AgentEvent::AssistantMessageDelta { .. }
                    }
                )
            }) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        conn.request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::InterruptAgentRun { run_id: run.id },
        )))
        .result
        .unwrap();
        let settled = await_settled(&conn, run.id);
        assert_eq!(settled.state, AgentRunState::Cancelled);
        assert!(
            !backend.has_live_acp_process(session.id),
            "wedged child must be killed and cleared after cancel grace"
        );
        backend.shutdown().unwrap();
    }

    // 3. R6: Turn inactivity timeout kills wedged harness and fails run cleanly.
    {
        let cmd = stub_command_with_env(&[("LOOM_ACP_STUB_SCENARIO", "hang")]);
        let backend = new_test_backend(Some(cmd));
        backend.set_acp_timeouts(Duration::from_secs(30), Duration::from_millis(350));
        let conn = backend.connect();
        negotiate_all(&conn);
        let workspace_id = create_workspace(&conn, "TurnTimeout");
        let session = create_acp_session(&conn, workspace_id, "TurnTimeout", None);
        let run = start_run(&conn, session.id, "Hang until timeout", "default");
        let settled = await_settled(&conn, run.id);
        assert_eq!(settled.state, AgentRunState::Failed);
        assert!(settled.summary.unwrap_or_default().contains("timed out"));
        assert!(!backend.has_live_acp_process(session.id));
        backend.shutdown().unwrap();
    }

    // 4. Harness exits mid-run (crash -> visible activity + Failed)
    {
        let cmd = stub_command_with_env(&[("LOOM_ACP_STUB_SCENARIO", "crash")]);
        let backend = new_test_backend(Some(cmd));
        let conn = backend.connect();
        negotiate_all(&conn);
        let workspace_id = create_workspace(&conn, "Crash");
        let session = create_acp_session(&conn, workspace_id, "Crash", None);
        let run = start_run(&conn, session.id, "Crash now", "default");
        let settled = await_settled(&conn, run.id);
        assert_eq!(settled.state, AgentRunState::Failed);
        let summary = settled.summary.unwrap_or_default();
        assert!(
            summary.contains("42") || summary.contains("fatal harness crash"),
            "{summary}"
        );
        backend.shutdown().unwrap();
    }

    // 5. R9: Secrets emitted on harness stderr are redacted before reaching activities/journal.
    {
        let cmd = stub_command_with_env(&[("LOOM_ACP_STUB_SCENARIO", "secret_stderr")]);
        let backend = new_test_backend(Some(cmd));
        let conn = backend.connect();
        negotiate_all(&conn);
        let workspace_id = create_workspace(&conn, "SecretStderr");
        let session = create_acp_session(&conn, workspace_id, "SecretStderr", None);
        let run = start_run(&conn, session.id, "Log secrets", "default");
        let settled = await_settled(&conn, run.id);
        assert_eq!(settled.state, AgentRunState::Completed);
        let proj = run_projection(&conn, run.id);
        let serialized = serde_json::to_string(&proj.activities).unwrap();
        assert!(serialized.contains("acp_stderr"));
        assert!(!serialized.contains("ya29.a0AfH6SM_secret_oauth_token_123"));
        assert!(!serialized.contains("AIzaSyD1234567890abcdef1234567890"));
        assert!(!serialized.contains("4/0AX_secret_code"));
        assert!(!serialized.contains("tok_xyz"));
        assert!(!serialized.contains("AIzaSyAnotherSecretKey9999999"));
        assert!(serialized.contains("[REDACTED]"));
        backend.shutdown().unwrap();
    }

    // 6. Unadvertised auth method -> clear failure as session activity with gemini-api-key hint
    {
        let cmd = stub_command_with_env(&[("LOOM_ACP_STUB_AUTH_METHODS", "gemini-api-key")]);
        let backend = new_test_backend(Some(cmd));
        let conn = backend.connect();
        negotiate_all(&conn);
        let workspace_id = create_workspace(&conn, "UnadvertisedAuth");
        let session = create_acp_session(
            &conn,
            workspace_id,
            "UnadvertisedAuth",
            Some("oauth-personal"),
        );
        let run = start_run(&conn, session.id, "Hi", "default");
        let settled = await_settled(&conn, run.id);
        assert_eq!(settled.state, AgentRunState::Failed);
        let summary = settled.summary.unwrap_or_default();
        assert!(
            summary.contains("oauth-personal") && summary.contains("gemini-api-key"),
            "{summary}"
        );
        backend.shutdown().unwrap();
    }

    // 7. Missing binary -> clear failure as session activity with gemini-api-key hint
    {
        let backend = new_test_backend(Some("/nonexistent/loom-missing-agy-binary".to_owned()));
        let conn = backend.connect();
        negotiate_all(&conn);
        let workspace_id = create_workspace(&conn, "MissingBin");
        let session = create_acp_session(&conn, workspace_id, "MissingBin", None);
        let run = start_run(&conn, session.id, "Hi", "default");
        let settled = await_settled(&conn, run.id);
        assert_eq!(settled.state, AgentRunState::Failed);
        let summary = settled.summary.unwrap_or_default();
        assert!(summary.contains("gemini-api-key"), "{summary}");
        backend.shutdown().unwrap();
    }
}

fn test_live_agy_initialize_opt_in() {
    // Opt-in only: never runs in CI and never performs sign-in.
    if std::env::var("LOOM_LIVE_AGY_TEST").as_deref() != Ok("1") {
        return;
    }
    let backend: Arc<InProcessBackend> = InProcessBackend::new();
    let conn = backend.connect();
    negotiate_all(&conn);
    let probe = match conn
        .request(RequestEnvelope::new(ClientRequest::Provider(
            ProviderRequest::ProbeAcpHarness {
                harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                auth_method: None,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Provider(ProviderResponse::AcpHarnessProbe(probe)) => probe,
        other => panic!("unexpected live probe: {other:?}"),
    };
    assert!(probe.available, "live agy probe failed: {:?}", probe.error);
    assert_eq!(probe.protocol_version, Some(1));
}

fn main() {
    if std::env::var_os("LOOM_ACP_STUB_MODE").is_some() {
        loom_harness::acp::stub::run_stub_stdio();
        return;
    }
    let tests: &[(&str, fn())] = &[
        (
            "capability_gate_and_authorization_refuse_unprivileged_clients",
            test_capability_gate_and_authorization_refuse_unprivileged_clients,
        ),
        (
            "probe_discovers_catalog_and_sanitizes_command_display",
            test_probe_discovers_catalog_and_sanitizes_command_display,
        ),
        (
            "interactive_only_guard_refuses_headless_partial_auto_approve_and_ci",
            test_interactive_only_guard_refuses_headless_partial_auto_approve_and_ci,
        ),
        (
            "full_acp_turn_multi_turn_model_switch_fork_profile_reuse_and_delete",
            test_full_acp_turn_multi_turn_model_switch_fork_profile_reuse_and_delete,
        ),
        (
            "server_restart_rehydrates_acp_runs_with_load_session_recap_and_pending_approval",
            test_server_restart_rehydrates_acp_runs_with_load_session_recap_and_pending_approval,
        ),
        (
            "permission_allow_deny_cancel_and_timeout_through_approval_ui",
            test_permission_allow_deny_cancel_and_timeout_through_approval_ui,
        ),
        (
            "cancel_wedged_timeout_crash_auth_missing_redaction_and_shutdown",
            test_cancel_wedged_timeout_crash_auth_missing_redaction_and_shutdown,
        ),
        (
            "live_agy_initialize_opt_in",
            test_live_agy_initialize_opt_in,
        ),
    ];
    if std::env::args().any(|arg| arg == "--list") {
        for (name, _) in tests {
            println!("{name}: test");
        }
        return;
    }
    for (name, test_fn) in tests {
        test_fn();
        println!("test {name} ... ok");
    }
}
