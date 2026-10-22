//! In-process tests: the durable session goal.
//!
//! A session's goal is backend-owned text. `/goal` sets it, a first run adopts
//! its task when the session has none, and the manager is told the goal it
//! owns. The value survives a restart and reaches every session read.

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
            goal: goal.to_owned(),
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
                if goal == "Ship the durable goal"
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
    backend.shutdown().unwrap();
}

#[test]
fn a_manager_run_names_the_goal_after_restart_and_resume() {
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
        await_settled_run(&connection, run_id);
        backend.flush().unwrap();

        // The assembled run configuration is what the provider request is
        // built from, and it carries the goal outside the compactable
        // transcript.
        let persisted = FilePersistence::open(&path).unwrap();
        let runtime_config = persisted.load_run_runtime_config(run_id).unwrap().unwrap();
        let system_instructions = runtime_config.system_instructions.unwrap();
        assert!(system_instructions.contains("The project goal is: Ship the durable goal"));
        drop(persisted);
        backend.shutdown().unwrap();
    }

    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate_m3(&connection);
    // Resuming the run rebuilds its task, including the instructions the goal
    // was folded into.
    let state = connection.run_handle(run_id).unwrap().state();
    assert!(
        state.task.system_instructions.as_deref().is_some_and(
            |instructions| instructions.contains("The project goal is: Ship the durable goal")
        ),
        "a resumed run still names the session goal"
    );
    backend.shutdown().unwrap();
    fs::remove_file(path).unwrap();
}
