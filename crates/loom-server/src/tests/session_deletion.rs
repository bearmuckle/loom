//! In-process tests: archived-session deletion.

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
    assert_eq!(session.id, session_id);
    assert_eq!(session.state, AgentSessionState::Archived);
}

/// Drives a delete request through the protocol.
fn delete(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
    force: bool,
) -> Result<ServerResponse> {
    connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::DeleteAgentSession { session_id, force },
        )))
        .result
}

/// Filesystem root of one session under the backend session root base.
fn session_root(
    backend: &InProcessBackend,
    workspace_id: WorkspaceId,
    session_id: AgentSessionId,
) -> PathBuf {
    backend
        .session_root_base
        .join(workspace_id.to_string())
        .join(session_id.to_string())
}

/// Creates one session inside an existing workspace.
fn create_session_in(
    connection: &InProcessConnection,
    workspace_id: WorkspaceId,
    name: &str,
) -> AgentSessionSnapshot {
    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id,
            name: name.to_owned(),
        },
    )));
    let Ok(ServerResponse::Session(SessionResponse::AgentSessionCreated(session))) = created.result
    else {
        panic!("unexpected session response");
    };
    session
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
    let session = create_session_in(connection, workspace.id, name);
    (workspace, session)
}

/// Asserts that a deletion response names exactly the requested session.
fn assert_deleted(response: ServerResponse, session_id: AgentSessionId) {
    assert!(
        matches!(
            response,
            ServerResponse::Session(SessionResponse::AgentSessionDeleted { session_id: deleted })
                if deleted == session_id
        ),
        "unexpected delete response"
    );
}

/// Adds one delegated code child with its own linked worktree below
/// `parent_session_id`, mirroring [`project_code_child_fixture`] one level
/// deeper. The returned checkout is a linked worktree of the parent checkout.
fn add_code_child(
    fixture: &ProjectCodeChildFixture,
    parent_session_id: AgentSessionId,
    parent_repository_id: RepositoryId,
    base_revision: &str,
    name: &str,
) -> (
    AgentSessionId,
    loom_core::TaskId,
    ProjectWorktreeRecord,
    PathBuf,
) {
    let backend = &fixture.backend;
    let workspace_id = backend
        .sessions()
        .unwrap()
        .get(parent_session_id)
        .unwrap()
        .workspace_id;
    let task_id = loom_core::TaskId::new();
    let child_session_id = AgentSessionId::new();
    let created_at = Timestamp::now();
    let child_snapshot = AgentSessionSnapshot {
        id: child_session_id,
        workspace_id,
        name: name.to_owned(),
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
    };
    let task = loom_core::DelegatedTaskRecord {
        task_id,
        project_id: fixture.project_id,
        requester_session_id: parent_session_id,
        target_session_id: child_session_id,
        child_name: name.to_owned(),
        intent: "Hold a checkout that must be removed deepest-first".to_owned(),
        model_id: "deterministic/demo".to_owned(),
        context_references: Vec::new(),
        dependencies: Vec::new(),
        code_change: true,
        permissions: loom_core::ProjectAgentPermissions::default(),
        status: loom_core::DelegatedTaskStatus::Completed,
        created_at,
        updated_at: created_at,
    };
    let mut worktree = ProjectWorktreeRecord {
        project_id: fixture.project_id,
        task_id,
        parent_session_id,
        child_session_id,
        parent_repository_id,
        child_repository_id: RepositoryId::new(),
        relative_path: format!("project-worktrees/{task_id}"),
        worktree_name: format!("loom-child-{task_id}"),
        branch_name: format!("loom/project-child-{task_id}"),
        base_revision: base_revision.to_owned(),
        result_revision: None,
        integrated_revision: None,
        status: ProjectWorktreeStatus::Creating,
        conflict_paths: Vec::new(),
        error: None,
        cleanup_disposition: None,
        created_at,
        updated_at: created_at,
    };
    persistence(backend)
        .create_project_child_with_worktree(
            RequestId::new(),
            &child_snapshot,
            backend.sessions().unwrap().next_sequence().next(),
            &task,
            &worktree,
        )
        .unwrap();
    let (_, event) = backend
        .sessions()
        .unwrap()
        .create_in_workspace_with_id(workspace_id, child_session_id, name.to_owned())
        .unwrap();
    backend.journal().unwrap().append_session(event);
    fixture
        .connection
        .ensure_project_worktree_ready(&mut worktree)
        .unwrap();
    let checkout = fixture
        .connection
        .session_filesystem(child_session_id)
        .unwrap()
        .root()
        .join(&worktree.relative_path);
    (child_session_id, task_id, worktree, checkout)
}

#[test]
fn deleting_an_archived_session_is_durable_and_removes_its_filesystem_root() {
    let temp = workspace();
    let path = temp.join("state.sqlite");
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let (workspace_record, session) =
        create_workspace_and_session(&connection, "Standalone history");
    let root = session_root(&backend, workspace_record.id, session.id);
    fs::write(root.join("fs").join("kept.txt"), "kept\n").unwrap();

    archive(&connection, session.id);
    assert_deleted(delete(&connection, session.id, false).unwrap(), session.id);
    assert_eq!(
        backend
            .sessions()
            .unwrap()
            .get(session.id)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert!(!root.exists());
    assert!(
        !persistence(&backend)
            .load_sessions()
            .unwrap()
            .unwrap()
            .sessions
            .contains_key(&session.id)
    );
    backend.flush().unwrap();
    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);

    // Reopening proves the deletion is durable: neither the row nor the
    // filesystem root comes back.
    let reopened = InProcessBackend::new_persistent(&path).unwrap();
    assert_eq!(
        reopened
            .sessions()
            .unwrap()
            .get(session.id)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert!(
        !persistence(&reopened)
            .load_sessions()
            .unwrap()
            .unwrap()
            .sessions
            .contains_key(&session.id)
    );
    assert!(!root.exists());
    reopened.shutdown().unwrap();
    drop(reopened);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn deleting_a_non_archived_session_is_refused_and_changes_nothing() {
    let temp = workspace();
    let path = temp.join("state.sqlite");
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let (workspace_record, session) = create_workspace_and_session(&connection, "Live history");
    let root = session_root(&backend, workspace_record.id, session.id);

    for force in [false, true] {
        let error = delete(&connection, session.id, force).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidState);
        assert_eq!(error.message, "only archived agent sessions can be deleted");
        assert!(!error.retryable);
    }
    assert_eq!(
        backend.sessions().unwrap().get(session.id).unwrap().state,
        AgentSessionState::Idle
    );
    assert!(root.exists());
    assert!(
        persistence(&backend)
            .load_sessions()
            .unwrap()
            .unwrap()
            .sessions
            .contains_key(&session.id)
    );
    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn deleting_an_archived_project_root_cascades_to_every_descendant() {
    let fixture = project_code_child_fixture();
    let child_session_id = fixture.worktree.child_session_id;
    let workspace_id = fixture
        .backend
        .sessions()
        .unwrap()
        .get(fixture.root_id)
        .unwrap()
        .workspace_id;
    let root_dir = session_root(&fixture.backend, workspace_id, fixture.root_id);
    let child_dir = session_root(&fixture.backend, workspace_id, child_session_id);

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
        AgentSessionState::Archived,
        "archiving a project root must archive its descendants"
    );

    assert_deleted(
        delete(&fixture.connection, fixture.root_id, false).unwrap(),
        fixture.root_id,
    );

    let sessions = fixture.backend.sessions().unwrap();
    assert_eq!(
        sessions.get(fixture.root_id).unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(
        sessions.get(child_session_id).unwrap_err().code,
        ErrorCode::NotFound
    );
    drop(sessions);
    assert!(!root_dir.exists());
    assert!(!child_dir.exists());
    assert!(!fixture.child_checkout.exists());

    let rows = persistence(&fixture.backend);
    let durable = rows.load_sessions().unwrap().unwrap();
    assert!(!durable.sessions.contains_key(&fixture.root_id));
    assert!(!durable.sessions.contains_key(&child_session_id));
    assert!(
        rows.list_project_tasks(fixture.project_id)
            .unwrap()
            .is_empty()
    );
    assert!(
        rows.load_project_worktree_by_task(fixture.task_id)
            .unwrap()
            .is_none()
    );
    assert!(
        rows.load_project_snapshot(fixture.project_id)
            .unwrap()
            .is_none_or(|project| project.agents.is_empty())
    );
    assert!(
        rows.load_recent_feed_events(child_session_id, 16)
            .unwrap()
            .is_empty()
    );
    assert!(
        rows.load_feed_events_since(Some(child_session_id), None)
            .unwrap()
            .is_empty()
    );

    // A restart must not restore any of the cascaded state.
    let database_path = fixture.temp.join("state.sqlite");
    drop(fixture.connection);
    fixture.backend.shutdown().unwrap();
    drop(fixture.backend);
    let reopened = InProcessBackend::new_persistent(&database_path).unwrap();
    for session_id in [fixture.root_id, child_session_id] {
        assert_eq!(
            reopened
                .sessions()
                .unwrap()
                .get(session_id)
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
    }
    let reopened_rows = persistence(&reopened);
    assert!(
        !reopened_rows
            .load_sessions()
            .unwrap()
            .unwrap()
            .sessions
            .contains_key(&fixture.root_id)
    );
    assert!(
        reopened_rows
            .list_project_tasks(fixture.project_id)
            .unwrap()
            .is_empty()
    );
    assert!(
        reopened_rows
            .load_project_worktree_by_task(fixture.task_id)
            .unwrap()
            .is_none()
    );
    assert!(!fixture.child_checkout.exists());
    assert!(!root_dir.exists());
    reopened.shutdown().unwrap();
    drop(reopened);
    let _ = fs::remove_dir_all(&fixture.temp);
    let _ = fs::remove_dir_all(&fixture.source);
}

#[test]
fn deleting_an_archived_child_session_prunes_its_worktree_registration() {
    let fixture = project_code_child_fixture();
    let child_session_id = fixture.worktree.child_session_id;
    complete_project_code_child(&fixture);
    archive(&fixture.connection, child_session_id);

    // The parent checkout registers the child checkout before the deletion.
    assert!(
        fixture
            .parent_git
            .open_linked_worktree(&fixture.worktree.worktree_name, &fixture.child_checkout)
            .is_ok()
    );

    assert_deleted(
        delete(&fixture.connection, child_session_id, false).unwrap(),
        child_session_id,
    );

    // The registration is pruned together with the checkout, and no stale
    // `.git/worktrees/<name>` directory survives.
    assert_eq!(
        fixture
            .parent_git
            .open_linked_worktree(&fixture.worktree.worktree_name, &fixture.child_checkout)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    let git_dir = fixture.parent_git.root().join(".git");
    assert!(
        git_dir.is_dir(),
        "parent checkout should use a .git directory"
    );
    assert!(
        !git_dir
            .join("worktrees")
            .join(&fixture.worktree.worktree_name)
            .exists()
    );
    assert!(!fixture.child_checkout.exists());

    // The surviving parent session keeps its repository and hierarchy.
    assert!(
        fixture
            .backend
            .sessions()
            .unwrap()
            .get(fixture.root_id)
            .is_ok()
    );
    assert!(
        fixture
            .backend
            .sessions()
            .unwrap()
            .get(child_session_id)
            .is_err()
    );
    assert_eq!(
        fixture.parent_git.status().unwrap().head.as_deref(),
        Some(fixture.worktree.base_revision.as_str())
    );
    let rows = persistence(&fixture.backend);
    let project = rows
        .load_project_snapshot(fixture.project_id)
        .unwrap()
        .expect("the surviving parent keeps the project hierarchy");
    assert_eq!(project.agents.len(), 1);
    assert_eq!(project.agents[0].session_id, fixture.root_id);
    assert!(project.worktrees.is_empty());
    assert!(
        rows.load_project_worktree_by_task(fixture.task_id)
            .unwrap()
            .is_none()
    );
    fixture.shutdown();
}

#[test]
fn deleting_a_depth_three_project_removes_worktrees_deepest_first() {
    let fixture = project_code_child_fixture();
    let child_session_id = fixture.worktree.child_session_id;
    let workspace_id = fixture
        .backend
        .sessions()
        .unwrap()
        .get(fixture.root_id)
        .unwrap()
        .workspace_id;
    let root_dir = session_root(&fixture.backend, workspace_id, fixture.root_id);
    let child_dir = session_root(&fixture.backend, workspace_id, child_session_id);
    let (grandchild_session_id, grandchild_task_id, grandchild_worktree, grandchild_checkout) =
        add_code_child(
            &fixture,
            child_session_id,
            fixture.worktree.child_repository_id,
            &fixture.worktree.base_revision,
            "Depth three",
        );
    let grandchild_dir = session_root(&fixture.backend, workspace_id, grandchild_session_id);
    assert!(fixture.child_checkout.exists());
    assert!(grandchild_checkout.exists());

    complete_project_code_child(&fixture);
    archive(&fixture.connection, fixture.root_id);
    for session_id in [fixture.root_id, child_session_id, grandchild_session_id] {
        assert_eq!(
            fixture
                .backend
                .sessions()
                .unwrap()
                .get(session_id)
                .unwrap()
                .state,
            AgentSessionState::Archived
        );
    }

    // The grandchild checkout is a linked worktree of the child checkout, so a
    // shallow-first removal would destroy the repository that still has to
    // unregister the grandchild and abort the request. A successful cascade
    // therefore proves the worktrees were removed deepest-first.
    assert_deleted(
        delete(&fixture.connection, fixture.root_id, false).unwrap(),
        fixture.root_id,
    );

    for session_id in [fixture.root_id, child_session_id, grandchild_session_id] {
        assert_eq!(
            fixture
                .backend
                .sessions()
                .unwrap()
                .get(session_id)
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
    }
    assert!(!root_dir.exists());
    assert!(!child_dir.exists());
    assert!(!grandchild_dir.exists());
    assert!(!fixture.child_checkout.exists());
    assert!(!grandchild_checkout.exists());

    let rows = persistence(&fixture.backend);
    assert!(
        rows.load_project_worktree_by_task(fixture.task_id)
            .unwrap()
            .is_none()
    );
    assert!(
        rows.load_project_worktree_by_task(grandchild_task_id)
            .unwrap()
            .is_none()
    );
    let durable = rows.load_sessions().unwrap().unwrap();
    assert!(!durable.sessions.contains_key(&fixture.root_id));
    assert!(!durable.sessions.contains_key(&child_session_id));
    assert!(!durable.sessions.contains_key(&grandchild_session_id));
    assert_eq!(
        grandchild_worktree.child_session_id, grandchild_session_id,
        "the grandchild worktree must belong to the grandchild session"
    );
    fixture.shutdown();
}

#[test]
fn a_dirty_linked_worktree_refuses_deletion_until_forced() {
    let fixture = project_code_child_fixture();
    let child_session_id = fixture.worktree.child_session_id;
    complete_project_code_child(&fixture);
    archive(&fixture.connection, fixture.root_id);
    fs::write(fixture.child_checkout.join("uncommitted.txt"), "dirty\n").unwrap();

    let error = delete(&fixture.connection, fixture.root_id, false).unwrap_err();
    assert_eq!(error.code, ErrorCode::Conflict);
    assert!(
        error.message.contains("has changes"),
        "unexpected message: {}",
        error.message
    );

    // The refused request keeps the checkout, every row, and both sessions.
    assert!(fixture.child_checkout.exists());
    for session_id in [fixture.root_id, child_session_id] {
        assert!(
            fixture.backend.sessions().unwrap().get(session_id).is_ok(),
            "session {session_id} must survive a refused deletion"
        );
    }
    let rows = persistence(&fixture.backend);
    assert!(
        rows.load_project_worktree_by_task(fixture.task_id)
            .unwrap()
            .is_some()
    );
    assert!(rows.load_delegated_task(fixture.task_id).unwrap().is_some());
    assert!(
        rows.load_sessions()
            .unwrap()
            .unwrap()
            .sessions
            .contains_key(&fixture.root_id)
    );

    assert_deleted(
        delete(&fixture.connection, fixture.root_id, true).unwrap(),
        fixture.root_id,
    );
    assert!(!fixture.child_checkout.exists());
    assert!(
        fixture
            .backend
            .sessions()
            .unwrap()
            .get(fixture.root_id)
            .is_err()
    );
    assert!(
        fixture
            .backend
            .sessions()
            .unwrap()
            .get(child_session_id)
            .is_err()
    );
    assert!(
        persistence(&fixture.backend)
            .load_project_worktree_by_task(fixture.task_id)
            .unwrap()
            .is_none()
    );
    fixture.shutdown();
}

#[test]
fn a_project_with_a_non_terminal_child_task_is_refused() {
    let fixture = project_code_child_fixture();
    let child_session_id = fixture.worktree.child_session_id;
    complete_project_code_child(&fixture);
    archive(&fixture.connection, fixture.root_id);
    // Put the archived child task back into a non-terminal state, as a
    // restarted schedule would.
    persistence(&fixture.backend)
        .update_delegated_task_status(
            fixture.task_id,
            loom_core::DelegatedTaskStatus::Queued,
            Timestamp::now(),
        )
        .unwrap();

    let error = delete(&fixture.connection, fixture.root_id, false).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidState);
    assert_eq!(
        error.message,
        "finish or cancel every child task before deleting this project"
    );

    // Nothing changes: sessions, checkout, worktree row, and task stay.
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
    assert!(
        fixture
            .backend
            .sessions()
            .unwrap()
            .get(child_session_id)
            .is_ok()
    );
    assert!(fixture.child_checkout.exists());
    let rows = persistence(&fixture.backend);
    assert!(
        rows.load_project_worktree_by_task(fixture.task_id)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        rows.load_delegated_task(fixture.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Queued
    );
    assert!(
        rows.load_sessions()
            .unwrap()
            .unwrap()
            .sessions
            .contains_key(&child_session_id)
    );
    assert!(
        fixture
            .parent_git
            .open_linked_worktree(&fixture.worktree.worktree_name, &fixture.child_checkout)
            .is_ok()
    );
    fixture.shutdown();
}

#[test]
fn deleting_archived_sessions_requires_durable_storage() {
    let ephemeral = InProcessBackend::new();
    let connection = ephemeral.connect();
    let discovered = connection
        .request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::DiscoverCapabilities,
        )))
        .result
        .unwrap();
    assert!(matches!(
        discovered,
        ServerResponse::Control(ControlResponse::Capabilities(result))
            if !result.capabilities.contains(Capability::DeleteAgentSession)
    ));
    negotiate(&connection);
    let (_, session) = create_workspace_and_session(&connection, "Ephemeral history");
    archive(&connection, session.id);

    // A client that still asks for the capability, for example across a
    // protocol version skew, reaches the durable-storage guard instead.
    *connection.negotiated_capabilities().unwrap() =
        Some(CapabilitySet::new([Capability::DeleteAgentSession]));
    let error = delete(&connection, session.id, false).unwrap_err();
    assert_eq!(error.code, ErrorCode::UnsupportedCapability);
    assert_eq!(
        error.message,
        "deleting archived sessions requires durable storage"
    );
    assert_eq!(
        ephemeral.sessions().unwrap().get(session.id).unwrap().state,
        AgentSessionState::Archived
    );

    // The same request is advertised and accepted with durable storage.
    let temp = workspace();
    let path = temp.join("state.sqlite");
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    let discovered = connection
        .request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::DiscoverCapabilities,
        )))
        .result
        .unwrap();
    assert!(matches!(
        discovered,
        ServerResponse::Control(ControlResponse::Capabilities(result))
            if result.capabilities.contains(Capability::DeleteAgentSession)
    ));
    negotiate(&connection);
    let (_, session) = create_workspace_and_session(&connection, "Durable history");
    archive(&connection, session.id);
    assert_deleted(delete(&connection, session.id, false).unwrap(), session.id);

    backend.shutdown().unwrap();
    drop(backend);
    let _ = fs::remove_dir_all(&ephemeral.session_root_base);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn deleting_a_session_removes_its_feed_rows() {
    let temp = workspace();
    let path = temp.join("state.sqlite");
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let (workspace_record, deleted_session) =
        create_workspace_and_session(&connection, "Deleted feed");
    let survivor = create_session_in(&connection, workspace_record.id, "Surviving feed");
    for (session_id, name) in [
        (deleted_session.id, "Deleted feed renamed"),
        (survivor.id, "Surviving feed renamed"),
    ] {
        connection
            .request(RequestEnvelope::new(ClientRequest::Session(
                SessionRequest::RenameAgentSession {
                    session_id,
                    name: name.to_owned(),
                },
            )))
            .result
            .unwrap();
    }
    backend.flush().unwrap();

    let rows = persistence(&backend);
    assert!(
        !rows
            .load_recent_feed_events(deleted_session.id, 16)
            .unwrap()
            .is_empty(),
        "the deleted session must have feed rows to remove"
    );
    assert!(
        !rows
            .load_feed_events_since(Some(deleted_session.id), None)
            .unwrap()
            .is_empty()
    );

    archive(&connection, deleted_session.id);
    assert_deleted(
        delete(&connection, deleted_session.id, false).unwrap(),
        deleted_session.id,
    );
    assert_eq!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Events(
                EventsRequest::GetRecentSessionEvents {
                    session_id: deleted_session.id,
                    limit: 16,
                },
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );

    let workspace_events = connection
        .request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id: None,
                workspace_id: Some(workspace_record.id),
                after_sequence: None,
                stream_epoch: None,
            },
        )))
        .result
        .unwrap();
    let events = match workspace_events {
        ServerResponse::Events(EventsResponse::WorkspaceEvents { events, .. })
        | ServerResponse::Events(EventsResponse::WorkspaceEventsSnapshot { events, .. }) => events,
        response => panic!("unexpected workspace events response: {response:?}"),
    };
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                WorkspaceFeedEvent::Session(envelope)
                    if envelope.session_id == deleted_session.id
            ))
            .count(),
        0,
        "the workspace feed must not serve events of a deleted session"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            WorkspaceFeedEvent::Session(envelope) if envelope.session_id == survivor.id
        )),
        "the surviving session keeps its feed events"
    );

    // The durable feed rows are gone as well.
    assert!(
        rows.load_recent_feed_events(deleted_session.id, 16)
            .unwrap()
            .is_empty()
    );
    assert!(
        rows.load_feed_events_since(Some(deleted_session.id), None)
            .unwrap()
            .is_empty()
    );
    assert!(
        rows.load_feed_workspace_events_since(workspace_record.id, None)
            .unwrap()
            .iter()
            .all(|event| !matches!(
                event,
                WorkspaceFeedEvent::Session(envelope)
                    if envelope.session_id == deleted_session.id
            ))
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn deleting_a_session_keeps_the_node_clone_cache() {
    let source = git_repository();
    let temp = workspace();
    let path = temp.join("state.sqlite");
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);

    // Seed the node cache with a mirror of a GitHub-shaped repository, then
    // attach a session checkout from that mirror without the network.
    let url = "https://github.com/owner/cached.git";
    let mirror = backend.repository_mirror_path(url).unwrap();
    GitService::create_mirror(&source, &mirror, url).unwrap();
    backend
        .register_cloned_repository(&ClonedRepository {
            full_name: "owner/cached".to_owned(),
            clone_url: url.to_owned(),
            branch: Some("main".to_owned()),
            last_used_at: Timestamp::from_unix_millis(1),
        })
        .unwrap();
    assert_eq!(backend.cached_repositories().unwrap().len(), 1);

    let (workspace_record, session) =
        create_workspace_and_session(&connection, "Cached clone session");
    let attached = connection.request(RequestEnvelope::new(ClientRequest::Repository(
        RepositoryRequest::AttachSessionRepository {
            session_id: session.id,
            source: url.to_owned(),
            path: "repo".to_owned(),
            revision: None,
            reuse_local: true,
        },
    )));
    let Ok(ServerResponse::Repository(RepositoryResponse::SessionRepositoryAttached(repository))) =
        attached.result
    else {
        panic!("expected a cached attachment");
    };
    assert_eq!(repository.source, "cached");

    archive(&connection, session.id);
    assert_deleted(delete(&connection, session.id, false).unwrap(), session.id);

    // The session is gone while the shared node cache stays.
    assert!(
        !session_root(&backend, workspace_record.id, session.id).exists(),
        "the deleted session root must be gone"
    );
    assert!(mirror.is_dir(), "the clone cache must survive a deletion");
    let cached = backend.cached_repositories().unwrap();
    assert_eq!(cached.len(), 1);
    assert_eq!(cached[0].full_name, "owner/cached");

    // The cache is also durable across a restart.
    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    let reopened = InProcessBackend::new_persistent(&path).unwrap();
    assert!(mirror.is_dir());
    assert_eq!(reopened.cached_repositories().unwrap().len(), 1);
    reopened.shutdown().unwrap();
    drop(reopened);
    let _ = fs::remove_dir_all(temp);
    let _ = fs::remove_dir_all(source);
}

#[test]
fn a_locked_linked_worktree_refuses_deletion_until_forced() {
    let fixture = project_code_child_fixture();
    let child_session_id = fixture.worktree.child_session_id;
    complete_project_code_child(&fixture);
    // The Git CLI names a linked worktree after its checkout path, while the
    // durable record keeps the libgit2 worktree name, so lock by path.
    git_in(
        fixture.parent_git.root(),
        &["worktree", "lock", fixture.child_checkout.to_str().unwrap()],
    );
    assert!(
        fixture
            .parent_git
            .linked_worktree_is_locked(&fixture.worktree.worktree_name)
            .unwrap()
    );
    archive(&fixture.connection, fixture.root_id);

    let error = delete(&fixture.connection, fixture.root_id, false).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidState);
    assert!(
        error.message.contains("is locked"),
        "unexpected message: {}",
        error.message
    );
    assert!(fixture.child_checkout.exists());
    assert!(
        fixture
            .backend
            .sessions()
            .unwrap()
            .get(fixture.root_id)
            .is_ok()
    );
    assert!(
        persistence(&fixture.backend)
            .load_project_worktree_by_task(fixture.task_id)
            .unwrap()
            .is_some()
    );

    assert_deleted(
        delete(&fixture.connection, fixture.root_id, true).unwrap(),
        fixture.root_id,
    );
    assert!(!fixture.child_checkout.exists());
    assert!(
        fixture
            .backend
            .sessions()
            .unwrap()
            .get(fixture.root_id)
            .is_err()
    );
    assert!(
        fixture
            .backend
            .sessions()
            .unwrap()
            .get(child_session_id)
            .is_err()
    );
    assert!(
        persistence(&fixture.backend)
            .load_project_worktree_by_task(fixture.task_id)
            .unwrap()
            .is_none()
    );
    fixture.shutdown();
}
