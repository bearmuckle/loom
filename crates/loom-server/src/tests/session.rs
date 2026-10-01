//! In-process tests: session.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn idempotency_cache_keeps_uuidv7_horizon_and_bounds_uuidv4_compatibility() {
    let now = Timestamp::now();
    let mut current = BTreeMap::new();
    for _ in 0..LEGACY_IDEMPOTENCY_RETENTION + 1 {
        let request_id = RequestId::new();
        current.insert(
            request_id,
            IdempotencyRecord {
                created_at: now,
                expires_at: request_id.issued_at_unix_millis().map(|issued_at| {
                    Timestamp::from_unix_millis(
                        issued_at + IDEMPOTENCY_RETENTION.as_millis() as u64,
                    )
                }),
                request: ClientRequest::Workspace(WorkspaceRequest::ListWorkspaces),
                response: ResponseEnvelope::success(
                    request_id,
                    ServerResponse::Workspace(WorkspaceResponse::WorkspaceConfigUpdated),
                ),
            },
        );
    }
    let expired_id = request_id_with_issued_at(
        now.as_unix_millis()
            .saturating_sub(IDEMPOTENCY_RETENTION.as_millis() as u64)
            .saturating_sub(1),
    );
    current.insert(
        expired_id,
        IdempotencyRecord {
            created_at: now,
            expires_at: Some(now),
            request: ClientRequest::Workspace(WorkspaceRequest::ListWorkspaces),
            response: ResponseEnvelope::success(
                expired_id,
                ServerResponse::Workspace(WorkspaceResponse::WorkspaceConfigUpdated),
            ),
        },
    );
    trim_idempotency_cache(&mut current);
    assert_eq!(current.len(), LEGACY_IDEMPOTENCY_RETENTION + 1);
    assert!(!current.contains_key(&expired_id));

    let mut legacy = BTreeMap::new();
    for _ in 0..LEGACY_IDEMPOTENCY_RETENTION + 1 {
        let request_id = RequestId::from_uuid(uuid::Uuid::new_v4());
        legacy.insert(
            request_id,
            IdempotencyRecord {
                created_at: now,
                expires_at: None,
                request: ClientRequest::Workspace(WorkspaceRequest::ListWorkspaces),
                response: ResponseEnvelope::success(
                    request_id,
                    ServerResponse::Workspace(WorkspaceResponse::WorkspaceConfigUpdated),
                ),
            },
        );
    }
    trim_idempotency_cache(&mut legacy);
    assert_eq!(legacy.len(), LEGACY_IDEMPOTENCY_RETENTION);
}

#[test]
fn bounded_review_text_is_unicode_safe_and_session_auth_errors_are_structured() {
    assert_eq!(bounded_review_text("short", 5), "short");
    assert_eq!(
        bounded_review_text("éclair", 2),
        "é\n...[review output truncated]"
    );
    assert_eq!(
        bounded_review_text("éclair", 1),
        "\n...[review output truncated]"
    );
    let error = unauthorized_session(AgentSessionId::new());
    assert_eq!(error.code, ErrorCode::AuthorizationDenied);
    assert!(!error.retryable);
    assert!(error.message.contains("not authorized for session"));
}

#[test]
fn explicit_limits_and_context_inspection_are_durable_protocol_state() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m3(&connection);
    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Limited workspace".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        workspace.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "limited run".to_owned(),
        },
    )));
    let session_id = match session.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRunWithOptions {
            session_id,
            task: "limited".to_owned(),
            model: ModelId::new("deterministic/demo"),
            system_instructions: Some("system".to_owned()),
            repository_instructions: Some("repository".to_owned()),
            limits: loom_core::SessionLimits {
                max_tool_calls: Some(0),
                ..Default::default()
            },
            context: ContextAssemblyOptions {
                context_window: Some(1_024),
                max_input_tokens: Some(512),
                reserved_output_tokens: Some(128),
            },
        },
    )));
    let run_id = match started.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    assert_eq!(
        await_settled_run(&connection, run_id).state,
        AgentRunState::Failed
    );
    let events = match connection
        .request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id: Some(session_id),
                workspace_id: None,
                after_sequence: None,
                stream_epoch: None,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) => events,
        response => panic!("unexpected response: {response:?}"),
    };
    assert!(events.iter().any(|event| {
        matches!(
            &event.event,
            ServerEvent::Agent {
                event: AgentEvent::RunLimitReached { status, .. }
            } if status.exceeded.contains(&loom_core::LimitKind::ToolCalls)
        )
    }));
    let usage = connection.request(RequestEnvelope::new(ClientRequest::Usage(
        UsageRequest::GetSessionUsage { session_id },
    )));
    let ServerResponse::Usage(UsageResponse::SessionUsage { usage, .. }) = usage.result.unwrap()
    else {
        panic!("unexpected session usage response");
    };
    assert_eq!(usage.tool_calls, 0);
    let context = connection.request(RequestEnvelope::new(ClientRequest::Context(
        ContextRequest::InspectAgentContext { run_id },
    )));
    assert_eq!(context.result.unwrap_err().code, ErrorCode::InvalidState);
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}

#[test]
fn archiving_a_session_with_a_deferred_run_stops_it() {
    let persistence =
        std::env::temp_dir().join(format!("loom-deferred-archive-{}.db", uuid::Uuid::new_v4()));
    let (session_id, run_id) = {
        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let workspace = match connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateWorkspace {
                    name: "Deferred archive".to_owned(),
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
            response => panic!("unexpected workspace response: {response:?}"),
        };
        let session_id = match connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateAgentSessionInWorkspace {
                    workspace_id: workspace.id,
                    name: "deferred".to_owned(),
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
            response => panic!("unexpected session response: {response:?}"),
        };
        // Require approval so the run parks in a non-terminal state that is
        // deferred on restore.
        assert!(
            connection
                .request(RequestEnvelope::new(ClientRequest::Session(
                    SessionRequest::SetSessionApprovalPolicy {
                        session_id,
                        policy: ApprovalPolicy::default(),
                        auto_approve_actions: Some(false),
                    },
                )))
                .result
                .is_ok()
        );
        let run_id = match connection
            .request(RequestEnvelope::new(ClientRequest::Run(
                RunRequest::StartSessionAgentRun {
                    session_id,
                    task: "create a demo file".to_owned(),
                    model: ModelId::new("deterministic/demo"),
                    system_instructions: None,
                    repository_instructions: None,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
            response => panic!("unexpected run response: {response:?}"),
        };
        await_settled_run(&connection, run_id);
        backend.flush().unwrap();
        backend.shutdown().unwrap();
        (session_id, run_id)
    };
    let backend = InProcessBackend::new_persistent(&persistence).unwrap();
    assert!(
        backend.runs().unwrap().is_empty(),
        "a non-terminal run is deferred on restore, so no handle is registered"
    );
    let connection = backend.connect();
    negotiate_m3(&connection);
    let archived = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::ArchiveAgentSession { session_id },
    )));
    assert!(matches!(
        archived.result,
        Ok(ServerResponse::Session(
            SessionResponse::AgentSessionArchived(_)
        ))
    ));
    assert_eq!(
        backend
            .persistence
            .as_ref()
            .unwrap()
            .load_run_summary(run_id)
            .unwrap()
            .unwrap()
            .snapshot
            .state,
        AgentRunState::Cancelled,
        "the deferred run must be stopped before the session is archived"
    );
    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    let _ = fs::remove_file(&persistence);
}

#[test]
fn archiving_a_session_with_a_stale_active_state_recovers() {
    let persistence =
        std::env::temp_dir().join(format!("loom-stale-archive-{}.db", uuid::Uuid::new_v4()));
    let backend = InProcessBackend::new_persistent(&persistence).unwrap();
    let connection = backend.connect();
    negotiate_m3(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Stale archive".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let session_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "stale".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    // Simulate a session left active by an earlier crash: the state says
    // Executing but there is no run at all.
    connection
        .backend
        .sessions()
        .unwrap()
        .transition(session_id, AgentSessionState::Executing)
        .unwrap();
    let archived = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::ArchiveAgentSession { session_id },
    )));
    assert!(matches!(
        archived.result,
        Ok(ServerResponse::Session(
            SessionResponse::AgentSessionArchived(_)
        ))
    ));
    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    let _ = fs::remove_file(&persistence);
}
