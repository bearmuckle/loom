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
    ApprovalPolicy, Capability, CapabilitySet, ErrorCode, SessionBackendDescriptor, WorkspaceId,
};
use loom_model::{AgentRunState, ModelId};
use loom_protocol::{
    ActivityDetail, AgentEvent, CURRENT_PROTOCOL_VERSION, ClientRequest, ControlRequest,
    EventsRequest, EventsResponse, ProviderRequest, ProviderResponse, RequestEnvelope, RunRequest,
    RunResponse, ServerEvent, ServerResponse, SessionRequest, SessionResponse, WorkspaceRequest,
    WorkspaceResponse,
};
use loom_server::{InProcessBackend, InProcessConnection};
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
            Capability::StartAgentRun,
            Capability::ReadAgentRun,
            Capability::ControlAgentRun,
            Capability::ApproveAgentAction,
            Capability::ConfigureApprovalPolicy,
            Capability::SubscribeSessionEvents,
            Capability::ReadUsage,
            Capability::ListProviders,
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

fn test_capability_gate_refuses_older_clients() {
    let backend = InProcessBackend::new();
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
        ],
    );
    let workspace_id = create_workspace(&old_client, "Compat");

    let err = old_client
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAcpAgentSessionInWorkspace {
                workspace_id,
                name: "ACP".to_owned(),
                harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                command: Some(stub_command()),
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
                command: Some(stub_command()),
                auth_method: None,
            },
        )))
        .result
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::CapabilityDenied);

    // Even if another newer client created an ACP session, an older client
    // without AcpAgentSessions cannot start a run in it.
    let new_client = backend.connect();
    negotiate_all(&new_client);
    let acp_session = match new_client
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAcpAgentSessionInWorkspace {
                workspace_id,
                name: "ACP".to_owned(),
                harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                command: Some(stub_command()),
                auth_method: None,
                interactive: true,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(s)) => s,
        other => panic!("unexpected session: {other:?}"),
    };

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
}

fn test_probe_discovers_catalog_and_policy_notices() {
    let backend = InProcessBackend::new();
    let conn = backend.connect();
    negotiate_all(&conn);

    let probe = match conn
        .request(RequestEnvelope::new(ClientRequest::Provider(
            ProviderRequest::ProbeAcpHarness {
                harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                command: Some(stub_command()),
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
    let notices = probe.notices.join("\n").to_ascii_lowercase();
    assert!(notices.contains("terms"));
    assert!(notices.contains("interactions"));
    assert!(notices.contains("not a google-registered acp client"));
}

fn test_interactive_only_guard_refuses_headless_and_auto_approve() {
    let backend = InProcessBackend::new();
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
                command: Some(stub_command()),
                auth_method: None,
                interactive: false,
            },
        )))
        .result
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRequest);
    assert!(err.message.contains("interactive-only"));

    // Interactive session starts with auto_approve_actions = false and refuses enabling auto-approve.
    let session = match conn
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAcpAgentSessionInWorkspace {
                workspace_id,
                name: "Interactive".to_owned(),
                harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                command: Some(stub_command()),
                auth_method: None,
                interactive: true,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(s)) => s,
        other => panic!("unexpected session: {other:?}"),
    };

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
}

fn test_full_acp_turn_mapping_client_info_no_prompt_injection_and_persistence() {
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

    let backend = InProcessBackend::new_persistent(&db_path).unwrap();
    let conn = backend.connect();
    negotiate_all(&conn);
    let workspace_id = create_workspace(&conn, "Antigravity");

    let session = match conn
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAcpAgentSessionInWorkspace {
                workspace_id,
                name: "AGY Session".to_owned(),
                harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                command: Some(cmd),
                auth_method: None,
                interactive: true,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(s)) => s,
        other => panic!("unexpected session: {other:?}"),
    };
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
    let projection = match conn
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::GetAgentRunSnapshot {
                run_id: run.id,
                activity_detail: ActivityDetail::Full,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Run(RunResponse::AgentRunSnapshot(proj)) => proj,
        other => panic!("unexpected projection: {other:?}"),
    };
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

    // Verify events included ReasoningDelta & PlanProposed & UsageUpdated.
    let events = session_events(&conn, session.id);
    assert!(events.iter().any(|e| matches!(
        e,
        ServerEvent::Agent {
            event: AgentEvent::ReasoningDelta { text, .. }
        } if text.contains("thinking through request")
    )));

    // Verify session record updated with protocol version, capabilities, remote_session_id, models.
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

    // Verify recorded JSON-RPC wire traffic: Loom's own clientInfo, no Loom system prompt injection, isolated profile dir.
    let recorded = fs::read_to_string(&record_path).unwrap();
    assert!(recorded.contains("\"name\":\"loom\""));
    assert!(!recorded.to_ascii_lowercase().contains("t3code"));
    assert!(!recorded.contains("SHOULD NOT BE INJECTED"));
    assert!(!recorded.contains("ALSO NOT INJECTED"));
    assert!(!recorded.contains("General agent instructions"));
    let lines = recorded
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .collect::<Vec<_>>();
    let started = &lines[0];
    let profile_dir = started["profile_dir"].as_str().expect("profile dir set");
    assert!(PathBuf::from(profile_dir).is_dir());

    // Verify SQLite persistence round-trip preserves session backend descriptor.
    drop(conn);
    drop(backend);
    let reopened = InProcessBackend::open_persistent(&db_path).unwrap();
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
    drop(reopened);
    let _ = fs::remove_dir_all(&dir);
}

fn test_permission_allow_deny_and_cancel_through_approval_ui() {
    for mode in ["allow", "deny", "cancel"] {
        let backend = InProcessBackend::new();
        let conn = backend.connect();
        negotiate_all(&conn);
        let workspace_id = create_workspace(&conn, &format!("Perm-{mode}"));
        let cmd = stub_command_with_env(&[("LOOM_ACP_STUB_SCENARIO", "permission")]);
        let session = match conn
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateAcpAgentSessionInWorkspace {
                    workspace_id,
                    name: format!("Perm-{mode}"),
                    harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                    command: Some(cmd),
                    auth_method: Some(ANTIGRAVITY_AUTH_GEMINI_API_KEY.to_owned()),
                    interactive: true,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Session(SessionResponse::AgentSessionCreated(s)) => s,
            other => panic!("unexpected session: {other:?}"),
        };

        let run = match conn
            .request(RequestEnvelope::new(ClientRequest::Run(
                RunRequest::StartSessionAgentRun {
                    session_id: session.id,
                    task: "Run cargo test".to_owned(),
                    model: ModelId::new("default"),
                    system_instructions: None,
                    repository_instructions: None,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Run(RunResponse::AgentRunStarted(r)) => r,
            other => panic!("unexpected run: {other:?}"),
        };

        let waiting = await_settled(&conn, run.id);
        assert_eq!(
            waiting.state,
            AgentRunState::AwaitingApproval,
            "mode={mode}"
        );

        let proj = match conn
            .request(RequestEnvelope::new(ClientRequest::Run(
                RunRequest::GetAgentRunSnapshot {
                    run_id: run.id,
                    activity_detail: ActivityDetail::Full,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Run(RunResponse::AgentRunSnapshot(proj)) => proj,
            other => panic!("unexpected proj: {other:?}"),
        };
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
                let final_proj = match conn
                    .request(RequestEnvelope::new(ClientRequest::Run(
                        RunRequest::GetAgentRunSnapshot {
                            run_id: run.id,
                            activity_detail: ActivityDetail::Full,
                        },
                    )))
                    .result
                    .unwrap()
                {
                    ServerResponse::Run(RunResponse::AgentRunSnapshot(p)) => p,
                    other => panic!("unexpected proj: {other:?}"),
                };
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
                let final_proj = match conn
                    .request(RequestEnvelope::new(ClientRequest::Run(
                        RunRequest::GetAgentRunSnapshot {
                            run_id: run.id,
                            activity_detail: ActivityDetail::Full,
                        },
                    )))
                    .result
                    .unwrap()
                {
                    ServerResponse::Run(RunResponse::AgentRunSnapshot(p)) => p,
                    other => panic!("unexpected proj: {other:?}"),
                };
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
            _ => unreachable!(),
        }
    }
}

fn test_cancel_mid_turn_crash_unadvertised_auth_and_missing_binary() {
    let backend = InProcessBackend::new();
    let conn = backend.connect();
    negotiate_all(&conn);
    let workspace_id = create_workspace(&conn, "Errors");

    // 1. Cancel mid-turn
    {
        let cmd = stub_command_with_env(&[("LOOM_ACP_STUB_SCENARIO", "cancel")]);
        let session = match conn
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateAcpAgentSessionInWorkspace {
                    workspace_id,
                    name: "Cancel".to_owned(),
                    harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                    command: Some(cmd),
                    auth_method: None,
                    interactive: true,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Session(SessionResponse::AgentSessionCreated(s)) => s,
            other => panic!("{other:?}"),
        };
        let run = match conn
            .request(RequestEnvelope::new(ClientRequest::Run(
                RunRequest::StartSessionAgentRun {
                    session_id: session.id,
                    task: "Long task".to_owned(),
                    model: ModelId::new("default"),
                    system_instructions: None,
                    repository_instructions: None,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Run(RunResponse::AgentRunStarted(r)) => r,
            other => panic!("{other:?}"),
        };
        // Wait briefly for streaming to start, then interrupt.
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
    }

    // 2. Harness exits mid-run (crash -> visible activity + Failed)
    {
        let cmd = stub_command_with_env(&[("LOOM_ACP_STUB_SCENARIO", "crash")]);
        let session = match conn
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateAcpAgentSessionInWorkspace {
                    workspace_id,
                    name: "Crash".to_owned(),
                    harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                    command: Some(cmd),
                    auth_method: None,
                    interactive: true,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Session(SessionResponse::AgentSessionCreated(s)) => s,
            other => panic!("{other:?}"),
        };
        let run = match conn
            .request(RequestEnvelope::new(ClientRequest::Run(
                RunRequest::StartSessionAgentRun {
                    session_id: session.id,
                    task: "Crash now".to_owned(),
                    model: ModelId::new("default"),
                    system_instructions: None,
                    repository_instructions: None,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Run(RunResponse::AgentRunStarted(r)) => r,
            other => panic!("{other:?}"),
        };
        let settled = await_settled(&conn, run.id);
        assert_eq!(settled.state, AgentRunState::Failed);
        let summary = settled.summary.unwrap_or_default();
        assert!(
            summary.contains("42") || summary.contains("fatal harness crash"),
            "{summary}"
        );
    }

    // 3. Unadvertised auth method -> clear failure as session activity with gemini-api-key hint
    {
        let cmd = stub_command_with_env(&[("LOOM_ACP_STUB_AUTH_METHODS", "gemini-api-key")]);
        let session = match conn
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateAcpAgentSessionInWorkspace {
                    workspace_id,
                    name: "UnadvertisedAuth".to_owned(),
                    harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                    command: Some(cmd),
                    auth_method: Some("oauth-personal".to_owned()),
                    interactive: true,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Session(SessionResponse::AgentSessionCreated(s)) => s,
            other => panic!("{other:?}"),
        };
        let run = match conn
            .request(RequestEnvelope::new(ClientRequest::Run(
                RunRequest::StartSessionAgentRun {
                    session_id: session.id,
                    task: "Hi".to_owned(),
                    model: ModelId::new("default"),
                    system_instructions: None,
                    repository_instructions: None,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Run(RunResponse::AgentRunStarted(r)) => r,
            other => panic!("{other:?}"),
        };
        let settled = await_settled(&conn, run.id);
        assert_eq!(settled.state, AgentRunState::Failed);
        let summary = settled.summary.unwrap_or_default();
        assert!(
            summary.contains("oauth-personal") && summary.contains("gemini-api-key"),
            "{summary}"
        );
    }

    // 4. Missing binary -> clear failure as session activity with gemini-api-key hint
    {
        let session = match conn
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateAcpAgentSessionInWorkspace {
                    workspace_id,
                    name: "MissingBin".to_owned(),
                    harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
                    command: Some("/nonexistent/loom-missing-agy-binary".to_owned()),
                    auth_method: None,
                    interactive: true,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Session(SessionResponse::AgentSessionCreated(s)) => s,
            other => panic!("{other:?}"),
        };
        let run = match conn
            .request(RequestEnvelope::new(ClientRequest::Run(
                RunRequest::StartSessionAgentRun {
                    session_id: session.id,
                    task: "Hi".to_owned(),
                    model: ModelId::new("default"),
                    system_instructions: None,
                    repository_instructions: None,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Run(RunResponse::AgentRunStarted(r)) => r,
            other => panic!("{other:?}"),
        };
        let settled = await_settled(&conn, run.id);
        assert_eq!(settled.state, AgentRunState::Failed);
        let summary = settled.summary.unwrap_or_default();
        assert!(summary.contains("gemini-api-key"), "{summary}");
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
                command: None,
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
            "capability_gate_refuses_older_clients",
            test_capability_gate_refuses_older_clients,
        ),
        (
            "probe_discovers_catalog_and_policy_notices",
            test_probe_discovers_catalog_and_policy_notices,
        ),
        (
            "interactive_only_guard_refuses_headless_and_auto_approve",
            test_interactive_only_guard_refuses_headless_and_auto_approve,
        ),
        (
            "full_acp_turn_mapping_client_info_no_prompt_injection_and_persistence",
            test_full_acp_turn_mapping_client_info_no_prompt_injection_and_persistence,
        ),
        (
            "permission_allow_deny_and_cancel_through_approval_ui",
            test_permission_allow_deny_and_cancel_through_approval_ui,
        ),
        (
            "cancel_mid_turn_crash_unadvertised_auth_and_missing_binary",
            test_cancel_mid_turn_crash_unadvertised_auth_and_missing_binary,
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
