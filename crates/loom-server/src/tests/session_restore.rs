//! In-process tests: archived-session restore.

#[allow(unused_imports)]
use super::support::*;
use super::*;

/// Durable rows of a persistent test backend.
fn persistence(backend: &InProcessBackend) -> &Arc<dyn Persistence> {
    backend
        .persistence
        .as_ref()
        .expect("test backend must be persistent")
}

/// Archives one session through the protocol.
fn archive(connection: &InProcessConnection, session_id: AgentSessionId) {
    let archived = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::ArchiveAgentSession { session_id },
    )));
    let Ok(ServerResponse::Session(SessionResponse::AgentSessionArchived(session))) =
        archived.result
    else {
        panic!("unexpected archive response for {session_id}");
    };
    assert_eq!(session.state, AgentSessionState::Archived);
}

/// Restores one session through the protocol.
fn restore(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
) -> Result<AgentSessionSnapshot> {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::RestoreAgentSession { session_id },
        )))
        .result?
    {
        ServerResponse::Session(SessionResponse::AgentSessionRestored(snapshot)) => Ok(snapshot),
        response => panic!("unexpected restore response: {response:?}"),
    }
}

/// Drives a delete request through the protocol.
fn delete(connection: &InProcessConnection, session_id: AgentSessionId) -> Result<ServerResponse> {
    connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::DeleteAgentSession {
                session_id,
                force: false,
            },
        )))
        .result
}

/// Creates a workspace with one standalone session in it.
fn create_workspace_and_session(
    connection: &InProcessConnection,
    name: &str,
) -> (WorkspaceRecord, AgentSessionSnapshot) {
    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: name.to_owned(),
        },
    )));
    let Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace))) =
        created.result
    else {
        panic!("unexpected workspace response");
    };
    let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: name.to_owned(),
        },
    )));
    let Ok(ServerResponse::Session(SessionResponse::AgentSessionCreated(session))) = session.result
    else {
        panic!("unexpected session response");
    };
    (workspace, session)
}

#[test]
fn restoring_an_archived_session_returns_it_to_idle_and_keeps_its_state() {
    let temp = workspace();
    let path = temp.join("state.sqlite");
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let (workspace_record, session) = create_workspace_and_session(&connection, "Restored history");
    let root = backend
        .session_root_base
        .join(workspace_record.id.to_string())
        .join(session.id.to_string());
    fs::write(root.join("fs").join("kept.txt"), "kept\n").unwrap();

    archive(&connection, session.id);
    assert_eq!(
        backend.sessions().unwrap().get(session.id).unwrap().state,
        AgentSessionState::Archived
    );

    let restored = restore(&connection, session.id).unwrap();
    assert_eq!(restored.id, session.id);
    assert_eq!(restored.state, AgentSessionState::Idle);
    assert_eq!(
        backend.sessions().unwrap().get(session.id).unwrap().state,
        AgentSessionState::Idle
    );
    // The filesystem stays where it was; a restore removes nothing.
    assert!(root.join("fs").join("kept.txt").exists());

    // The restore is journaled so a connected client learns the new state.
    let events = connection
        .request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetRecentSessionEvents {
                activity_detail: loom_protocol::ActivityDetail::Full,
                session_id: session.id,
                limit: 16,
            },
        )))
        .result
        .unwrap();
    let ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) = events else {
        panic!("unexpected session events response");
    };
    assert!(events.iter().any(|event| matches!(
        event.event,
        ServerEvent::AgentSessionRestored { session_id } if session_id == session.id
    )));

    // A restored session is not a sweep candidate even with a tiny window.
    connection
        .request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::SetArchiveRetentionPolicy {
                policy: loom_protocol::ArchiveRetentionPolicy {
                    retention_ms: Some(1),
                    force_discard_worktrees: true,
                    sub_agent_retention_ms: None,
                },
            },
        )))
        .result
        .unwrap();
    let report = backend.sweep_archive_retention().unwrap();
    assert!(!report.deleted_sessions.contains(&session.id));
    assert!(backend.sessions().unwrap().get(session.id).is_ok());

    // The restored state survives a restart on the same database.
    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    let reopened = InProcessBackend::new_persistent(&path).unwrap();
    assert_eq!(
        reopened.sessions().unwrap().get(session.id).unwrap().state,
        AgentSessionState::Idle
    );
    reopened.shutdown().unwrap();
    drop(reopened);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn restoring_a_session_that_is_not_archived_or_was_deleted_is_refused() {
    let temp = workspace();
    let path = temp.join("state.sqlite");
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let (_, session) = create_workspace_and_session(&connection, "Deleted history");

    let error = restore(&connection, session.id).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidState);
    assert_eq!(
        error.message,
        "only archived agent sessions can be restored"
    );
    assert!(!error.retryable);

    archive(&connection, session.id);
    delete(&connection, session.id).unwrap();
    // Deletion stays terminal: the row is gone, so a restore cannot find it.
    let error = restore(&connection, session.id).unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);
    assert_eq!(
        backend
            .sessions()
            .unwrap()
            .get(session.id)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn restoring_a_project_root_restores_the_archived_descendants() {
    let fixture = project_code_child_fixture();
    let child_session_id = fixture.worktree.child_session_id;
    complete_project_code_child(&fixture);
    archive(&fixture.connection, fixture.root_id);
    assert_eq!(
        fixture
            .backend
            .sessions()
            .unwrap()
            .get(child_session_id)
            .unwrap()
            .state,
        AgentSessionState::Archived
    );
    assert_eq!(
        persistence(&fixture.backend)
            .load_archived_project_members(fixture.root_id)
            .unwrap(),
        vec![(child_session_id, 2)]
    );

    let restored = restore(&fixture.connection, fixture.root_id).unwrap();
    assert_eq!(restored.id, fixture.root_id);
    assert_eq!(restored.state, AgentSessionState::Idle);
    for session_id in [fixture.root_id, child_session_id] {
        assert_eq!(
            fixture
                .backend
                .sessions()
                .unwrap()
                .get(session_id)
                .unwrap()
                .state,
            AgentSessionState::Idle,
            "session {session_id} must be restored with its root"
        );
    }
    assert!(fixture.child_checkout.exists());
    assert!(
        persistence(&fixture.backend)
            .load_archived_project_members(fixture.root_id)
            .unwrap()
            .is_empty(),
        "a restored tree clears its archived membership"
    );

    let database_path = fixture.temp.join("state.sqlite");
    drop(fixture.connection);
    fixture.backend.shutdown().unwrap();
    drop(fixture.backend);
    let reopened = InProcessBackend::new_persistent(&database_path).unwrap();
    for session_id in [fixture.root_id, child_session_id] {
        assert_eq!(
            reopened.sessions().unwrap().get(session_id).unwrap().state,
            AgentSessionState::Idle
        );
    }
    reopened.shutdown().unwrap();
    drop(reopened);
    let _ = fs::remove_dir_all(&fixture.temp);
    let _ = fs::remove_dir_all(&fixture.source);
}

#[test]
fn restoring_a_root_whose_descendant_was_swept_is_refused_and_changes_nothing() {
    let fixture = project_code_child_fixture();
    let child_session_id = fixture.worktree.child_session_id;
    complete_project_code_child(&fixture);
    archive(&fixture.connection, fixture.root_id);
    // The sub-agent window can sweep a descendant before the root's window
    // does; deleting it directly leaves the same durable outcome.
    delete(&fixture.connection, child_session_id).unwrap();
    assert_eq!(
        fixture
            .backend
            .sessions()
            .unwrap()
            .get(child_session_id)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );

    let error = restore(&fixture.connection, fixture.root_id).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidState);
    assert!(
        error.message.contains(&child_session_id.to_string()),
        "the refusal must name the missing session: {}",
        error.message
    );
    assert!(!error.retryable);

    // Nothing changed: the root is still archived and the record still names
    // the swept member, so the refusal repeats instead of half-restoring.
    assert_eq!(
        fixture
            .backend
            .sessions()
            .unwrap()
            .get(fixture.root_id)
            .unwrap()
            .state,
        AgentSessionState::Archived
    );
    assert_eq!(
        persistence(&fixture.backend)
            .load_archived_project_members(fixture.root_id)
            .unwrap(),
        vec![(child_session_id, 2)]
    );
    let error = restore(&fixture.connection, fixture.root_id).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidState);
    fixture.shutdown();
}

#[test]
fn a_restored_session_starts_a_new_run() {
    let temp = workspace();
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_id = ModelId::new("fixture/restored-session-run");
    let backend = InProcessBackend::with_provider_registry_persistent(
        scripted_project_provider_registry(&model.endpoint, model_id.clone()),
        temp.join("state.sqlite"),
    )
    .unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let (_, session) = create_workspace_and_session(&connection, "Run after restore");
    archive(&connection, session.id);
    restore(&connection, session.id).unwrap();

    let run_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRun {
                session_id: session.id,
                task: "Continue after the restore.".to_owned(),
                model: model_id.clone(),
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
    let turn = model.next_for_manager();
    model.respond_with_text(turn, "Resumed after the restore.");
    assert_eq!(
        await_settled_run(&connection, run_id).state,
        AgentRunState::Completed
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    drop(model);
    let _ = fs::remove_dir_all(temp);
}
