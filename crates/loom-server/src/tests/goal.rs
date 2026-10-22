//! In-process tests: the durable session goal.
//!
//! A session's goal is backend-owned text. `/goal` sets it, the value survives a
//! restart and reaches every session read, and a goal-driven run works against
//! it until the agent claims it is reached. A run's task is never promoted into
//! a goal.

#[allow(unused_imports)]
use super::support::*;
use super::*;

fn create_workspace_and_session(connection: &InProcessConnection) -> (WorkspaceId, AgentSessionId) {
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Goal workspace".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let session = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "New project".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session,
        response => panic!("unexpected session response: {response:?}"),
    };
    (workspace.id, session.id)
}

fn set_goal(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
    goal: &str,
) -> ResponseEnvelope {
    connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::SetAgentSessionGoal {
            session_id,
            goal: Some(goal.to_owned()),
        },
    )))
}

fn clear_goal(connection: &InProcessConnection, session_id: AgentSessionId) -> ResponseEnvelope {
    connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::SetAgentSessionGoal {
            session_id,
            goal: None,
        },
    )))
}

#[test]
fn a_goal_survives_restart_and_reaches_every_session_read() {
    let path = std::env::temp_dir().join(format!("loom-server-goal-{}.db", uuid::Uuid::new_v4()));
    let workspace_id;
    let session_id;
    {
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        let connection = backend.connect();
        negotiate_m3(&connection);
        (workspace_id, session_id) = create_workspace_and_session(&connection);

        let updated = set_goal(&connection, session_id, "  Ship the durable goal  ");
        let ServerResponse::Session(SessionResponse::AgentSessionGoalChanged(snapshot)) =
            updated.result.unwrap()
        else {
            panic!("unexpected goal response");
        };
        assert_eq!(snapshot.goal.as_deref(), Some("Ship the durable goal"));

        // The change is journalled like a rename, so a connected client can
        // apply it without refetching the session.
        let events = match connection
            .request(RequestEnvelope::new(ClientRequest::Events(
                EventsRequest::GetSessionEvents {
                    activity_detail: loom_protocol::ActivityDetail::Full,
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
            response => panic!("unexpected session event response: {response:?}"),
        };
        assert!(events.iter().any(|event| matches!(
            &event.event,
            ServerEvent::AgentSessionGoalChanged { goal, .. }
                if goal.as_deref() == Some("Ship the durable goal")
        )));

        let initial = match connection
            .request(RequestEnvelope::new(ClientRequest::Session(
                SessionRequest::GetAgentSessionInitialState {
                    session_id,
                    activity_detail: loom_protocol::ActivityDetail::Full,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Session(SessionResponse::AgentSessionInitialState(initial)) => initial,
            response => panic!("unexpected initial state response: {response:?}"),
        };
        assert_eq!(
            initial.projection.session.goal.as_deref(),
            Some("Ship the durable goal")
        );

        let listed = match connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::ListWorkspaceSessions {
                    workspace_id,
                    include_archived: false,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Session(SessionResponse::AgentSessions { sessions }) => sessions,
            response => panic!("unexpected session list response: {response:?}"),
        };
        assert_eq!(
            listed
                .iter()
                .find(|session| session.id == session_id)
                .and_then(|session| session.goal.as_deref()),
            Some("Ship the durable goal")
        );
        backend.shutdown().unwrap();
    }

    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate_m3(&connection);
    let snapshot = match connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::GetAgentSessionSnapshot {
                session_id,
                activity_detail: loom_protocol::ActivityDetail::Full,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionSnapshot(snapshot)) => snapshot,
        response => panic!("unexpected session snapshot response: {response:?}"),
    };
    assert_eq!(
        snapshot.session.goal.as_deref(),
        Some("Ship the durable goal"),
        "a restarted backend returns the stored goal"
    );
    backend.shutdown().unwrap();
    fs::remove_file(path).unwrap();
}

#[test]
fn clearing_a_goal_removes_it_from_every_session_read() {
    let path = std::env::temp_dir().join(format!(
        "loom-server-goal-clear-{}.db",
        uuid::Uuid::new_v4()
    ));
    let workspace_id;
    let session_id;
    {
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        let connection = backend.connect();
        negotiate_m3(&connection);
        (workspace_id, session_id) = create_workspace_and_session(&connection);
        set_goal(&connection, session_id, "Ship the durable goal")
            .result
            .unwrap();

        let cleared = clear_goal(&connection, session_id);
        let ServerResponse::Session(SessionResponse::AgentSessionGoalChanged(snapshot)) =
            cleared.result.unwrap()
        else {
            panic!("unexpected clear goal response");
        };
        assert_eq!(snapshot.goal, None);
        let cleared_sequence = connection.request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                activity_detail: loom_protocol::ActivityDetail::Full,
                session_id: Some(session_id),
                workspace_id: None,
                after_sequence: None,
                stream_epoch: None,
            },
        )));
        let ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) =
            cleared_sequence.result.unwrap()
        else {
            panic!("unexpected session event response");
        };
        // The clear is journalled with no goal, so a connected client removes
        // the goal line without refetching the session. The journal carries the
        // set first and the clear last.
        let goal_events = events
            .iter()
            .filter_map(|event| match &event.event {
                ServerEvent::AgentSessionGoalChanged { goal, .. } => Some(goal.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            goal_events,
            vec![Some("Ship the durable goal".to_owned()), None]
        );

        let listed = match connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::ListWorkspaceSessions {
                    workspace_id,
                    include_archived: false,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Session(SessionResponse::AgentSessions { sessions }) => sessions,
            response => panic!("unexpected session list response: {response:?}"),
        };
        assert_eq!(
            listed
                .iter()
                .find(|session| session.id == session_id)
                .and_then(|session| session.goal.as_deref()),
            None
        );
        // Clearing an already clear goal is rejected before any state change.
        assert_eq!(
            clear_goal(&connection, session_id).result.unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        backend.shutdown().unwrap();
    }

    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate_m3(&connection);
    let snapshot = match connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::GetAgentSessionSnapshot {
                session_id,
                activity_detail: loom_protocol::ActivityDetail::Full,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionSnapshot(snapshot)) => snapshot,
        response => panic!("unexpected session snapshot response: {response:?}"),
    };
    assert_eq!(
        snapshot.session.goal, None,
        "a cleared goal stays cleared after a restart"
    );
    backend.shutdown().unwrap();
    fs::remove_file(path).unwrap();
}

#[test]
fn invalid_goal_requests_leave_the_session_unchanged() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m3(&connection);
    let (_, session_id) = create_workspace_and_session(&connection);

    let blank = set_goal(&connection, session_id, "   ");
    assert_eq!(blank.result.unwrap_err().code, ErrorCode::InvalidRequest);
    let too_long = "x".repeat(loom_core::MAX_AGENT_SESSION_GOAL_BYTES + 1);
    let long = set_goal(&connection, session_id, &too_long);
    assert_eq!(long.result.unwrap_err().code, ErrorCode::InvalidRequest);
    let unknown = set_goal(&connection, AgentSessionId::new(), "A goal");
    assert_eq!(unknown.result.unwrap_err().code, ErrorCode::NotFound);

    let current = match connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::GetAgentSession { session_id },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSession(snapshot)) => snapshot,
        response => panic!("unexpected session response: {response:?}"),
    };
    assert_eq!(
        current.goal, None,
        "a rejected goal must not change the session"
    );

    connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::ArchiveAgentSession { session_id },
        )))
        .result
        .unwrap();
    let archived = set_goal(&connection, session_id, "A later goal");
    assert_eq!(archived.result.unwrap_err().code, ErrorCode::InvalidState);
    let archived_clear = clear_goal(&connection, session_id);
    assert_eq!(
        archived_clear.result.unwrap_err().code,
        ErrorCode::InvalidState
    );
    backend.shutdown().unwrap();
}

#[test]
fn a_run_works_against_the_goal_and_reports_it_after_restart() {
    let path =
        std::env::temp_dir().join(format!("loom-server-goal-run-{}.db", uuid::Uuid::new_v4()));
    let run_id;
    {
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let (_, session_id) = create_workspace_and_session(&connection);
        set_goal(&connection, session_id, "Ship the durable goal")
            .result
            .unwrap();
        let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRunWithOptions {
                session_id,
                task: "create a demo file".to_owned(),
                model: ModelId::new("deterministic/demo"),
                system_instructions: Some("Be concise.".to_owned()),
                repository_instructions: None,
                limits: loom_core::SessionLimits::default(),
                context: ContextAssemblyOptions::default(),
            },
        )));
        run_id = match started.result.unwrap() {
            ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
            response => panic!("unexpected run response: {response:?}"),
        };
        // The runtime works against the session goal, which stays outside the
        // compactable transcript.
        await_settled_run(&connection, run_id);
        let handle = connection.run_handle(run_id).unwrap();
        assert_eq!(
            handle.try_runtime().unwrap().goal(),
            Some("Ship the durable goal")
        );
        backend.flush().unwrap();
        backend.shutdown().unwrap();
    }

    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate_m3(&connection);
    // A restored run is told the durable session goal again, so it resumes
    // working toward the same objective instead of losing it with the process.
    let handle = connection.run_handle(run_id).unwrap();
    assert_eq!(
        handle.try_runtime().unwrap().goal(),
        Some("Ship the durable goal"),
        "a resumed run still works against the session goal"
    );
    backend.shutdown().unwrap();
    fs::remove_file(path).unwrap();
}

#[test]
fn a_run_task_is_not_promoted_into_a_goal() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m3(&connection);
    let (_, session_id) = create_workspace_and_session(&connection);
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id,
            task: "create a demo file".to_owned(),
            model: ModelId::new("deterministic/demo"),
            system_instructions: Some("Be concise.".to_owned()),
            repository_instructions: None,
        },
    )));
    let run_id = match started.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected run response: {response:?}"),
    };
    await_settled_run(&connection, run_id);

    let snapshot = match connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::GetAgentSessionSnapshot {
                session_id,
                activity_detail: loom_protocol::ActivityDetail::Full,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionSnapshot(snapshot)) => snapshot,
        response => panic!("unexpected session snapshot response: {response:?}"),
    };
    assert_eq!(
        snapshot.session.goal, None,
        "a run's task is not a goal; only /goal sets one"
    );
    backend.shutdown().unwrap();
}

#[test]
fn a_goal_set_while_a_run_is_registered_reaches_its_next_turn() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m3(&connection);
    let (_, session_id) = create_workspace_and_session(&connection);
    // A prompt for every non-read action stops the run after its first write,
    // so the goal can be changed while the run is registered.
    connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::SetSessionApprovalPolicy {
                session_id,
                policy: loom_core::ApprovalPolicy::default(),
                auto_approve_actions: Some(false),
            },
        )))
        .result
        .unwrap();
    set_goal(&connection, session_id, "Ship the durable goal")
        .result
        .unwrap();
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id,
            task: "create a demo file".to_owned(),
            model: ModelId::new("deterministic/demo"),
            system_instructions: Some("Be concise.".to_owned()),
            repository_instructions: None,
        },
    )));
    let run_id = match started.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected run response: {response:?}"),
    };
    let mut settled = await_settled_run(&connection, run_id);
    assert_eq!(settled.state, AgentRunState::AwaitingApproval);

    set_goal(&connection, session_id, "Fix the failing tests")
        .result
        .unwrap();

    while !matches!(
        settled.state,
        AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
    ) {
        let pending = connection
            .run_handle(run_id)
            .unwrap()
            .state()
            .pending_approval
            .expect("a stopped run with no pending call is not resumable here");
        connection
            .request(RequestEnvelope::new(ClientRequest::Run(
                RunRequest::ApproveAgentAction {
                    run_id,
                    attempt_id: settled.attempt_id,
                    expected_control_revision: settled.control_revision,
                    tool_call_id: pending.id,
                },
            )))
            .result
            .unwrap();
        settled = await_settled_run(&connection, run_id);
    }

    let handle = connection.run_handle(run_id).unwrap();
    assert_eq!(
        handle.try_runtime().unwrap().goal(),
        Some("Fix the failing tests"),
        "a goal changed during the run reaches the next model turn"
    );
    backend.shutdown().unwrap();
}
