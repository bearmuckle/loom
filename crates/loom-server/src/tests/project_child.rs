//! In-process tests: project child.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn m5_session_projections_reconnect_and_archive_authoritatively() {
    let root = git_repository();
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m5(&connection);
    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Navigator".to_owned(),
        },
    )));
    let Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace))) =
        created.result
    else {
        panic!("expected workspace creation");
    };
    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "Navigator session".to_owned(),
        },
    )));
    let session = match created.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot,
        response => panic!("unexpected response: {response:?}"),
    };

    let attached = connection.request(RequestEnvelope::new(ClientRequest::Repository(
        RepositoryRequest::AttachSessionRepository {
            session_id: session.id,
            source: root.display().to_string(),
            path: "repo".to_owned(),
            revision: None,
        },
    )));
    assert!(matches!(
        attached.result,
        Ok(ServerResponse::Repository(
            RepositoryResponse::SessionRepositoryAttached(_)
        ))
    ));
    let workspaces = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::ListWorkspaces,
    )));
    let ServerResponse::Workspace(WorkspaceResponse::Workspaces { workspaces }) =
        workspaces.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    assert_eq!(workspaces, vec![workspace.clone()]);

    let renamed = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::RenameAgentSession {
            session_id: session.id,
            name: "Renamed session".to_owned(),
        },
    )));
    let session = match renamed.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionRenamed(snapshot)) => snapshot,
        response => panic!("unexpected rename response: {response:?}"),
    };
    assert_eq!(session.name, "Renamed session");

    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id: session.id,
            task: "inspect the workspace".to_owned(),
            model: loom_model::ModelId::new("deterministic/demo"),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    let run_id = match started.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected run response: {response:?}"),
    };

    let snapshot = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::GetAgentSessionSnapshot {
            session_id: session.id,
        },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessionSnapshot(snapshot)) =
        snapshot.result.unwrap()
    else {
        panic!("unexpected session snapshot response");
    };
    assert_eq!(snapshot.session.id, session.id);
    assert_eq!(
        snapshot.active_run.as_ref().map(|run| run.run.id),
        Some(run_id)
    );
    assert!(snapshot.active_run.unwrap().plan.is_empty());

    let metadata = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::GetAgentSessionSnapshotMetadata {
            session_id: session.id,
        },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessionSnapshot(metadata)) =
        metadata.result.unwrap()
    else {
        panic!("unexpected metadata snapshot response");
    };
    assert_eq!(
        metadata.active_run.as_ref().map(|run| run.run.id),
        Some(run_id)
    );
    assert!(
        metadata
            .active_run
            .as_ref()
            .is_some_and(|run| run.messages.is_empty())
    );
    let run = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunSnapshot { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRunSnapshot(run)) = run.result.unwrap() else {
        panic!("unexpected run snapshot response");
    };
    assert_eq!(run.run.id, run_id);
    assert!(!run.messages.is_empty());

    let changes = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::GetSessionFilesystemChanges {
            session_id: session.id,
            after_sequence: None,
        },
    )));
    assert!(matches!(
        changes.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::SessionFilesystemChanges { .. }
        ))
    ));

    let archived = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::ArchiveAgentSession {
            session_id: session.id,
        },
    )));
    assert!(matches!(
        archived.result,
        Ok(ServerResponse::Session(
            SessionResponse::AgentSessionArchived(_)
        ))
    ));
    let sessions = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::ListWorkspaceSessions {
            workspace_id: workspace.id,
            include_archived: false,
        },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessions { sessions }) =
        sessions.result.unwrap()
    else {
        panic!("unexpected session list response");
    };
    assert!(sessions.is_empty());
    fs::remove_dir_all(&backend.session_root_base).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_snapshot_is_available_to_authorized_tokens_and_scoped_by_membership() {
    let backend = InProcessBackend::new();
    let unrestricted = backend.connect();
    negotiate(&unrestricted);
    let workspace = match unrestricted
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Project snapshots".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match unrestricted
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Project root".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(root)) => root,
        response => panic!("unexpected session response: {response:?}"),
    };
    let project_id = ProjectId::from_uuid(*root.id.as_uuid());
    let snapshot = unrestricted
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::GetProjectSnapshot { project_id },
        )))
        .result
        .unwrap();
    let ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot)) = snapshot else {
        panic!("expected project snapshot, received {snapshot:?}");
    };
    assert_eq!(snapshot.project_id, project_id);
    assert_eq!(snapshot.root_session_id, root.id);
    assert_eq!(snapshot.agents.len(), 1);
    assert_eq!(snapshot.agents[0].depth, 1);
    assert_eq!(snapshot.agents[0].session_id, root.id);

    let unknown_project_id = ProjectId::new();
    assert_eq!(
        unrestricted
            .request(RequestEnvelope::new(ClientRequest::Project(
                ProjectRequest::GetProjectSnapshot {
                    project_id: unknown_project_id,
                }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );

    let tokens = AuthTokenStore::new();
    let issued = tokens
        .issue(AuthorizationScope::for_sessions(
            [],
            backend.supported_capabilities.clone(),
        ))
        .unwrap();
    let scoped = backend.connect_authenticated(tokens.authenticate(&issued.token).unwrap());
    negotiate(&scoped);
    assert_eq!(
        scoped
            .request(RequestEnvelope::new(ClientRequest::Project(
                ProjectRequest::GetProjectSnapshot { project_id }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::AuthorizationDenied
    );
    let wrong_workspace = tokens
        .issue(AuthorizationScope::for_workspaces(
            [WorkspaceId::new()],
            backend.supported_capabilities.clone(),
        ))
        .unwrap();
    let workspace_scoped =
        backend.connect_authenticated(tokens.authenticate(&wrong_workspace.token).unwrap());
    negotiate(&workspace_scoped);
    assert_eq!(
        workspace_scoped
            .request(RequestEnvelope::new(ClientRequest::Project(
                ProjectRequest::GetProjectSnapshot { project_id }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::AuthorizationDenied
    );
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}

#[test]
fn archiving_project_requires_terminal_children_then_archives_the_tree() {
    let temp = std::env::temp_dir().join(format!(
        "loom-project-archive-policy-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let path = temp.join("state.sqlite");
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Archive policy".into(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Manager".into(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session,
        response => panic!("unexpected session response: {response:?}"),
    };
    let child_id = AgentSessionId::new();
    let created_at = Timestamp::now();
    let child = AgentSessionSnapshot {
        id: child_id,
        workspace_id: workspace.id,
        name: "Child".into(),
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
    };
    let task = loom_core::DelegatedTaskRecord {
        task_id: loom_core::TaskId::new(),
        project_id: ProjectId::from_uuid(*root.id.as_uuid()),
        requester_session_id: root.id,
        target_session_id: child_id,
        child_name: child.name.clone(),
        intent: "Wait for manager direction".into(),
        model_id: "deterministic/demo".into(),
        context_references: vec![],
        dependencies: vec![],
        code_change: false,
        permissions: loom_core::ProjectAgentPermissions::default(),
        status: loom_core::DelegatedTaskStatus::Queued,
        created_at,
        updated_at: created_at,
    };
    backend
        .persistence
        .as_ref()
        .unwrap()
        .create_project_child(
            RequestId::new(),
            &child,
            backend.sessions().unwrap().next_sequence().next(),
            &task,
        )
        .unwrap();
    let (_, created_event) = backend
        .sessions()
        .unwrap()
        .create_in_workspace_with_id(workspace.id, child_id, child.name.clone())
        .unwrap();
    backend.journal().unwrap().append_session(created_event);

    assert_eq!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Session(
                SessionRequest::ArchiveAgentSession {
                    session_id: root.id,
                }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::InvalidState
    );
    assert_eq!(
        backend.sessions().unwrap().get(root.id).unwrap().state,
        AgentSessionState::Idle
    );

    backend
        .persistence
        .as_ref()
        .unwrap()
        .update_delegated_task_status(
            task.task_id,
            loom_core::DelegatedTaskStatus::Cancelled,
            Timestamp::now(),
        )
        .unwrap();
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Session(SessionRequest::ArchiveAgentSession{
                session_id: root.id,
            })))
            .result,
        Ok(ServerResponse::Session(SessionResponse::AgentSessionArchived(session)))
            if session.state == AgentSessionState::Archived
    ));
    assert_eq!(
        backend.sessions().unwrap().get(child_id).unwrap().state,
        AgentSessionState::Archived
    );

    drop(connection);
    drop(backend);
    let _ = fs::remove_dir_all(temp);
    let _ = fs::remove_dir_all(path.with_extension("session-roots"));
    let _ = fs::remove_file(path.with_extension("credentials.json"));
}

#[test]
fn project_concurrency_limit_queues_additional_children() {
    let (endpoint, request_seen) = slow_model_endpoint();
    let path = std::env::temp_dir().join(format!(
        "loom-project-concurrency-{}.db",
        uuid::Uuid::new_v4()
    ));
    let backend = InProcessBackend::with_openai_compatible_persistent(
        endpoint,
        "test-key",
        ModelId::new("slow/model"),
        &path,
    )
    .unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Project concurrency".into(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Manager".into(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                    workspace_id: workspace.id,
                    config: WorkspaceConfig {
                        project_agent_concurrency: 1,
                        ..WorkspaceConfig::default()
                    },
                }
            ),))
            .result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::WorkspaceConfigUpdated
        ))
    ));

    let create_child = |child_name: &str| {
        connection.create_project_child(
            RequestId::new(),
            root,
            child_name.to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: format!("Review {child_name}"),
                model_id: "slow/model".into(),
                context_references: vec![],
                dependencies: vec![],
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions::default(),
            },
        )
    };

    let first = match create_child("worker-1").unwrap() {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, child }) => {
            (task, child)
        }
        response => panic!("unexpected first child response: {response:?}"),
    };
    request_seen
        .recv_timeout(Duration::from_secs(3))
        .expect("first child should begin its provider request");
    let second = match create_child("worker-2").unwrap() {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, child }) => {
            (task, child)
        }
        response => panic!("unexpected second child response: {response:?}"),
    };
    assert_eq!(first.0.status, loom_core::DelegatedTaskStatus::Running);
    assert_eq!(second.0.status, loom_core::DelegatedTaskStatus::Queued);
    assert_ne!(first.1.session_id, second.1.session_id);
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(
                ClientRequest::Run(RunRequest::StartSessionAgentRun{
                    session_id: second.1.session_id,
                    task: "Bypass the project queue".into(),
                    model: ModelId::new("slow/model"),
                    system_instructions: None,
                    repository_instructions: None,
                }),
            ))
            .result,
        Err(error) if error.code == ErrorCode::Conflict
    ));

    backend.shutdown().unwrap();
    drop(connection);
    drop(backend);
    let session_roots = path.with_extension("session-roots");
    let _ = fs::remove_dir_all(session_roots);
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(path.with_extension("credentials.json"));
}

#[test]
fn project_child_control_covers_lifecycle_and_parent_grants() {
    let temp = std::env::temp_dir().join(format!(
        "loom-project-child-control-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/project-child-control");
    let provider_registry =
        || scripted_project_provider_registry(&model_endpoint, model_id.clone());
    let backend = InProcessBackend::with_provider_registry_persistent(
        provider_registry(),
        temp.join("state.sqlite"),
    )
    .unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Project child control test".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let manager_session_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Manager".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    let child_session_id = AgentSessionId::new();
    let task_id = loom_core::TaskId::new();
    let project_id = ProjectId::from_uuid(*manager_session_id.as_uuid());
    let created_at = Timestamp::now();
    let child_name = "Queued child".to_owned();
    let child = AgentSessionSnapshot {
        id: child_session_id,
        workspace_id: workspace.id,
        name: child_name.clone(),
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
    };
    let task = loom_core::DelegatedTaskRecord {
        task_id,
        project_id,
        requester_session_id: manager_session_id,
        target_session_id: child_session_id,
        child_name,
        intent: "Remain queued until the manager continues the child".to_owned(),
        model_id: "missing-provider/model".to_owned(),
        context_references: vec![],
        dependencies: vec![],
        code_change: false,
        permissions: loom_core::ProjectAgentPermissions::default(),
        status: loom_core::DelegatedTaskStatus::Queued,
        created_at,
        updated_at: created_at,
    };
    let persistence = backend.persistence.as_ref().unwrap();
    persistence
        .create_project_child(
            RequestId::new(),
            &child,
            backend.sessions().unwrap().next_sequence().next(),
            &task,
        )
        .unwrap();
    let (_, created_event) = backend
        .sessions()
        .unwrap()
        .create_in_workspace_with_id(workspace.id, child_session_id, task.child_name.clone())
        .unwrap();
    backend.journal().unwrap().append_session(created_event);
    let filesystem = backend
        .create_session_filesystem(workspace.id, child_session_id)
        .unwrap();
    backend
        .session_filesystems()
        .unwrap()
        .insert(child_session_id, filesystem);
    backend
        .session_repositories()
        .unwrap()
        .insert(child_session_id, BTreeMap::new());

    let control_task = |task_id, action| {
        connection.request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::ControlProjectChild {
                project_id,
                manager_session_id,
                task_id,
                action,
            },
        )))
    };
    let control = |action| control_task(task_id, action);
    let continued = control(ProjectChildControlAction::Continue);
    let ServerResponse::Project(ProjectResponse::ProjectChildControlled { task, run }) =
        continued.result.unwrap()
    else {
        panic!("unexpected child continue response");
    };
    assert_eq!(task.status, loom_core::DelegatedTaskStatus::Blocked);
    assert!(run.is_none());

    let retried = control(ProjectChildControlAction::Continue);
    let ServerResponse::Project(ProjectResponse::ProjectChildControlled { task, run }) =
        retried.result.unwrap()
    else {
        panic!("unexpected blocked child continue response");
    };
    assert_eq!(task.status, loom_core::DelegatedTaskStatus::Blocked);
    assert!(run.is_none());

    for action in [
        ProjectChildControlAction::Pause,
        ProjectChildControlAction::Interrupt,
        ProjectChildControlAction::RetryFailedStep,
    ] {
        assert_eq!(
            control(action).result.unwrap_err().code,
            ErrorCode::InvalidState
        );
    }

    let resumable_session_id = AgentSessionId::new();
    let resumable_task = loom_core::DelegatedTaskRecord {
        task_id: loom_core::TaskId::new(),
        project_id,
        requester_session_id: manager_session_id,
        target_session_id: resumable_session_id,
        child_name: "Resumable child".to_owned(),
        intent: "Pause and then continue this child".to_owned(),
        model_id: model_id.as_str().to_owned(),
        context_references: vec![],
        dependencies: vec![],
        code_change: false,
        permissions: loom_core::ProjectAgentPermissions::default(),
        status: loom_core::DelegatedTaskStatus::Queued,
        created_at,
        updated_at: created_at,
    };
    let resumable_child = AgentSessionSnapshot {
        id: resumable_session_id,
        workspace_id: workspace.id,
        name: resumable_task.child_name.clone(),
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
    };
    persistence
        .create_project_child(
            RequestId::new(),
            &resumable_child,
            backend.sessions().unwrap().next_sequence().next(),
            &resumable_task,
        )
        .unwrap();
    let (_, created_event) = backend
        .sessions()
        .unwrap()
        .create_in_workspace_with_id(
            workspace.id,
            resumable_session_id,
            resumable_child.name.clone(),
        )
        .unwrap();
    backend.journal().unwrap().append_session(created_event);
    let filesystem = backend
        .create_session_filesystem(workspace.id, resumable_session_id)
        .unwrap();
    backend
        .session_filesystems()
        .unwrap()
        .insert(resumable_session_id, filesystem);
    backend
        .session_repositories()
        .unwrap()
        .insert(resumable_session_id, BTreeMap::new());
    connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::SetSessionApprovalPolicy {
                session_id: resumable_session_id,
                policy: ApprovalPolicy::default(),
                auto_approve_actions: Some(false),
            },
        )))
        .result
        .unwrap();

    let continue_resumable = || {
        connection.request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::ControlProjectChild {
                project_id,
                manager_session_id,
                task_id: resumable_task.task_id,
                action: ProjectChildControlAction::Continue,
            },
        )))
    };
    let started = continue_resumable();
    let ServerResponse::Project(ProjectResponse::ProjectChildControlled { run, .. }) =
        started.result.unwrap()
    else {
        panic!("unexpected resumable child response");
    };
    let run_id = run.expect("queued child should start a run").id;
    let first_model_turn = model.next_for_child();
    model.respond_with_tool(
        first_model_turn,
        "ask_user",
        serde_json::json!({"prompt": "Which option should I use?"}),
    );
    assert_eq!(
        await_settled_run(&connection, run_id).state,
        AgentRunState::NeedsInput
    );
    for action in [
        ProjectChildControlAction::Continue,
        ProjectChildControlAction::Pause,
    ] {
        assert_eq!(
            control_task(resumable_task.task_id, action)
                .result
                .unwrap_err()
                .code,
            ErrorCode::InvalidState
        );
    }
    let waiting_snapshot = match connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::GetAgentRun { run_id },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Run(RunResponse::AgentRun(snapshot)) => snapshot,
        response => panic!("unexpected waiting child response: {response:?}"),
    };
    connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::SendAgentMessage {
                run_id,
                attempt_id: waiting_snapshot.attempt_id,
                expected_control_revision: waiting_snapshot.control_revision,
                message: "Use the first option".to_owned(),
            },
        )))
        .result
        .unwrap();
    let approval_turn = model.next_for_child();
    model.respond_with_tool(
        approval_turn,
        "apply_patch",
        serde_json::json!({
            "path": "controlled-child.txt",
            "old_text": "before",
            "new_text": "after"
        }),
    );
    assert_eq!(
        await_settled_run(&connection, run_id).state,
        AgentRunState::AwaitingApproval
    );
    let paused = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::ControlProjectChild {
            project_id,
            manager_session_id,
            task_id: resumable_task.task_id,
            action: ProjectChildControlAction::Pause,
        },
    )));
    let ServerResponse::Project(ProjectResponse::ProjectChildControlled { run, .. }) =
        paused.result.unwrap()
    else {
        panic!("unexpected child pause response");
    };
    assert!(matches!(run, Some(run)
        if run.id == run_id && run.state == AgentRunState::Paused));
    let paused_again = control_task(resumable_task.task_id, ProjectChildControlAction::Pause);
    let ServerResponse::Project(ProjectResponse::ProjectChildControlled { run, .. }) =
        paused_again.result.unwrap()
    else {
        panic!("unexpected repeated child pause response");
    };
    assert!(matches!(run, Some(run)
        if run.id == run_id && run.state == AgentRunState::Paused));

    let resumed = continue_resumable();
    let ServerResponse::Project(ProjectResponse::ProjectChildControlled { run, .. }) =
        resumed.result.unwrap()
    else {
        panic!("unexpected child resume response");
    };
    assert!(matches!(run, Some(run)
        if run.id == run_id && run.state == AgentRunState::AwaitingApproval));
    let retry_while_awaiting_approval = control_task(
        resumable_task.task_id,
        ProjectChildControlAction::RetryFailedStep,
    );
    let ServerResponse::Project(ProjectResponse::ProjectChildControlled { run, .. }) =
        retry_while_awaiting_approval.result.unwrap()
    else {
        panic!("unexpected retry response for a run awaiting approval");
    };
    assert!(matches!(run, Some(run)
        if run.id == run_id && run.state == AgentRunState::AwaitingApproval));
    let approval_events = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: Some(resumable_session_id),
            workspace_id: None,
            after_sequence: None,
            stream_epoch: None,
        },
    )));
    let ServerResponse::Events(EventsResponse::SessionEvents {
        events: approval_events,
        ..
    }) = approval_events.result.unwrap()
    else {
        panic!("unexpected child approval events response");
    };
    let (approval_call_id, approval_attempt_id, approval_control_revision) = approval_events
        .iter()
        .find_map(|event| match &event.event {
            ServerEvent::Agent {
                event:
                    AgentEvent::ToolApprovalRequired {
                        call,
                        attempt_id,
                        control_revision,
                        ..
                    },
            } if call.name == "apply_patch" => Some((call.id, *attempt_id, *control_revision)),
            _ => None,
        })
        .expect("resumed child should retain its pending tool approval");
    connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::ApproveAgentAction {
                run_id,
                attempt_id: approval_attempt_id,
                expected_control_revision: approval_control_revision,
                tool_call_id: approval_call_id,
            },
        )))
        .result
        .unwrap();
    let completion_turn = model.next_for_child();
    model.respond_with_text(completion_turn, "The child completed after approval.");
    assert_eq!(
        await_settled_run(&connection, run_id).state,
        AgentRunState::Completed
    );
    for action in [
        ProjectChildControlAction::Continue,
        ProjectChildControlAction::Pause,
        ProjectChildControlAction::Interrupt,
        ProjectChildControlAction::RetryFailedStep,
    ] {
        assert_eq!(
            connection
                .request(RequestEnvelope::new(ClientRequest::Project(
                    ProjectRequest::ControlProjectChild {
                        project_id,
                        manager_session_id,
                        task_id: resumable_task.task_id,
                        action,
                    }
                )))
                .result
                .unwrap_err()
                .code,
            ErrorCode::InvalidState
        );
    }

    let grandchild_session_id = AgentSessionId::new();
    let grandchild_task = loom_core::DelegatedTaskRecord {
        task_id: loom_core::TaskId::new(),
        project_id,
        requester_session_id: child_session_id,
        target_session_id: grandchild_session_id,
        child_name: "Grandchild".to_owned(),
        intent: "Remain queued under the delegated manager".to_owned(),
        model_id: "missing-provider/model".to_owned(),
        context_references: vec![],
        dependencies: vec![],
        code_change: false,
        permissions: loom_core::ProjectAgentPermissions::default(),
        status: loom_core::DelegatedTaskStatus::Queued,
        created_at,
        updated_at: created_at,
    };
    let grandchild = AgentSessionSnapshot {
        id: grandchild_session_id,
        workspace_id: workspace.id,
        name: grandchild_task.child_name.clone(),
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
    };
    persistence
        .create_project_child(
            RequestId::new(),
            &grandchild,
            backend.sessions().unwrap().next_sequence().next(),
            &grandchild_task,
        )
        .unwrap();
    let (_, created_event) = backend
        .sessions()
        .unwrap()
        .create_in_workspace_with_id(workspace.id, grandchild_session_id, grandchild.name.clone())
        .unwrap();
    backend.journal().unwrap().append_session(created_event);
    let filesystem = backend
        .create_session_filesystem(workspace.id, grandchild_session_id)
        .unwrap();
    backend
        .session_filesystems()
        .unwrap()
        .insert(grandchild_session_id, filesystem);
    backend
        .session_repositories()
        .unwrap()
        .insert(grandchild_session_id, BTreeMap::new());
    let child_manager_control = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::ControlProjectChild {
            project_id,
            manager_session_id: child_session_id,
            task_id: grandchild_task.task_id,
            action: ProjectChildControlAction::Cancel,
        },
    )));
    assert_eq!(
        control_task(grandchild_task.task_id, ProjectChildControlAction::Cancel)
            .result
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest,
        "the root manager cannot control a non-direct descendant"
    );
    assert_eq!(
        child_manager_control.result.unwrap_err().code,
        ErrorCode::AuthorizationDenied,
        "a child manager without parent-granted control permission cannot control its child"
    );
    assert!(matches!(
        control(ProjectChildControlAction::Cancel).result,
        Ok(ServerResponse::Project(ProjectResponse::ProjectChildControlled{ task, run: None }))
            if task.status == loom_core::DelegatedTaskStatus::Cancelled
    ));
    assert_eq!(
        control(ProjectChildControlAction::Continue)
            .result
            .unwrap_err()
            .code,
        ErrorCode::InvalidState
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
}

#[cfg(unix)]
#[test]
fn cancelling_a_child_running_a_sleep_command_stops_promptly() {
    let temp = std::env::temp_dir().join(format!(
        "loom-child-sleep-cancel-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/child-sleep-cancel");
    let backend = InProcessBackend::with_provider_registry_persistent(
        scripted_project_provider_registry(&model_endpoint, model_id.clone()),
        &persistence_path,
    )
    .unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Child sleep cancel e2e".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Manager".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    let root_run_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRun {
                session_id: root,
                task: "Delegate a long sleep task.".to_owned(),
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
    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let persistence = backend.persistence.as_ref().unwrap();

    let root_delegate = model.next_for_manager();
    assert!(request_has_tool(
        &root_delegate.request,
        "delegate_project_task"
    ));
    model.respond_with_tool(
        root_delegate,
        "delegate_project_task",
        serde_json::json!({
            "child_name": "sleeper",
            "intent": "Run a long sleep command.",
            "model_id": model_id,
        }),
    );

    // The child's first model request only arrives once its task, session, and
    // run exist, so wait for it before looking the task up.
    let child_turn = model.next_for_child();
    let child_task = persistence
        .list_project_tasks(project_id)
        .unwrap()
        .into_iter()
        .find(|task| task.child_name == "sleeper")
        .expect("delegation should create the sleeper task");
    let child_run_id = persistence
        .load_latest_run_summary_for_session(child_task.target_session_id)
        .unwrap()
        .expect("child should have a durable run")
        .snapshot
        .id;

    model.respond_with_tool(
        child_turn,
        "run_command",
        serde_json::json!({"command": "sleep", "args": ["30"], "timeout_ms": 30_000}),
    );

    // Wait until the child is actually blocked inside the command. The activity
    // is in the live run handle because the step has not checkpointed yet.
    let child_handle = connection
        .backend
        .runs()
        .unwrap()
        .get(&child_run_id)
        .cloned()
        .expect("child run should be registered");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let running = child_handle.state().activities.iter().any(|activity| {
            activity.status == loom_protocol::AgentActivityStatus::Started
                && matches!(
                    &activity.data,
                    loom_protocol::AgentActivityData::Command { command, .. }
                        if command == "sleep"
                )
        });
        if running {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the child should start its sleep command; state={:?}",
            child_handle.snapshot().state,
        );
        thread::sleep(Duration::from_millis(10));
    }

    let started = Instant::now();
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::ControlProjectChild {
                project_id,
                manager_session_id: root,
                task_id: child_task.task_id,
                action: ProjectChildControlAction::Cancel,
            },
        )))
        .result
        .unwrap();
    assert!(matches!(
        response,
        ServerResponse::Project(ProjectResponse::ProjectChildControlled { .. })
    ));
    assert_eq!(
        await_settled_run(&connection, child_run_id).state,
        AgentRunState::Cancelled
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "cancelling a child must terminate its running command promptly"
    );

    // Finish the manager's pending turn so teardown does not wait on a held
    // model request.
    let root_followup = model.next_for_manager();
    model.respond_with_text(root_followup, "The sleeper was cancelled.");
    let _ = await_settled_run(&connection, root_run_id);

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn cancelling_project_child_cascades_deepest_first_and_survives_restart() {
    fn hold_model_stream_until_cancelled(request: ScriptedModelRequest) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let mut stream = request.stream;
            if stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .is_err()
            {
                return;
            }
            let _ = stream.flush();
            for _ in 0..3_000 {
                if stream
                    .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"holding\"}}]}\n\n")
                    .is_err()
                    || stream.flush().is_err()
                {
                    return;
                }
                thread::sleep(Duration::from_millis(20));
            }
        })
    }

    let temp = std::env::temp_dir().join(format!(
        "loom-project-cancel-cascade-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/project-cancel-cascade");
    let provider_registry =
        || scripted_project_provider_registry(&model_endpoint, model_id.clone());
    let backend =
        InProcessBackend::with_provider_registry_persistent(provider_registry(), &persistence_path)
            .unwrap();

    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Project cancellation cascade e2e".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Project root".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                    workspace_id: workspace.id,
                    config: WorkspaceConfig {
                        project_agent_concurrency: 2,
                        ..WorkspaceConfig::default()
                    },
                }
            ),))
            .result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::WorkspaceConfigUpdated
        ))
    ));

    let (manager_task, manager_session) = match connection
        .create_project_child(
            RequestId::new(),
            root,
            "manager".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Delegate one bounded investigation, then wait for direction.".to_owned(),
                model_id: model_id.as_str().to_owned(),
                context_references: vec![],
                dependencies: vec![],
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions {
                    delegation: true,
                    ..loom_core::ProjectAgentPermissions::default()
                },
            },
        )
        .unwrap()
    {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, child }) => {
            (task, child)
        }
        response => panic!("unexpected manager creation response: {response:?}"),
    };
    assert_eq!(manager_task.requester_session_id, root);
    assert_eq!(manager_session.depth, 2);

    let manager_delegate = model.next_for_child();
    assert!(request_has_tool(
        &manager_delegate.request,
        "delegate_project_task"
    ));
    let sibling_task = match connection
        .create_project_child(
            RequestId::new(),
            root,
            "sibling".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Hold one independent task while the manager delegates.".to_owned(),
                model_id: model_id.as_str().to_owned(),
                context_references: vec![],
                dependencies: vec![],
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions::default(),
            },
        )
        .unwrap()
    {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, .. }) => task,
        response => panic!("unexpected sibling task response: {response:?}"),
    };
    assert_eq!(sibling_task.status, loom_core::DelegatedTaskStatus::Running);
    let sibling_turn = model.next_for_child();
    assert!(!request_has_tool(
        &sibling_turn.request,
        "delegate_project_task"
    ));
    let sibling_stream = hold_model_stream_until_cancelled(sibling_turn);

    model.respond_with_tool(
        manager_delegate,
        "delegate_project_task",
        serde_json::json!({
            "child_name": "grandchild",
            "intent": "Run a short investigation and report the finding.",
            "model_id": model_id,
            "permissions": {}
        }),
    );

    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let persistence = backend.persistence.as_ref().unwrap();
    // Both workspace slots are occupied, so the nested child remains
    // queued while the manager is still active.
    let manager_turn = model.next_for_child();
    assert!(request_has_tool(
        &manager_turn.request,
        "delegate_project_task"
    ));
    let manager_stream = hold_model_stream_until_cancelled(manager_turn);
    let grandchild_task = persistence
        .list_project_tasks(project_id)
        .unwrap()
        .into_iter()
        .find(|task| task.requester_session_id == manager_session.session_id)
        .expect("manager delegation should create the grandchild task");
    let grandchild_agent = persistence
        .load_project_snapshot(project_id)
        .unwrap()
        .unwrap()
        .agents
        .into_iter()
        .find(|agent| agent.session_id == grandchild_task.target_session_id)
        .expect("grandchild should belong to the project hierarchy");
    assert_eq!(
        grandchild_agent.parent_session_id,
        Some(manager_session.session_id)
    );
    assert_eq!(grandchild_agent.depth, 3);
    let manager_run_id = persistence
        .load_latest_run_summary_for_session(manager_session.session_id)
        .unwrap()
        .expect("manager run should be durable")
        .snapshot
        .id;
    assert_eq!(
        grandchild_task.requester_session_id,
        manager_session.session_id
    );
    assert_eq!(
        grandchild_task.status,
        loom_core::DelegatedTaskStatus::Queued
    );
    assert!(
        persistence
            .load_latest_run_summary_for_session(grandchild_task.target_session_id)
            .unwrap()
            .is_none()
    );

    let sequence_before_cancel = backend.journal().unwrap().latest_sequence(None);
    backend
        .project_cancellation_failpoint
        .store(usize::MAX, Ordering::SeqCst);
    let cancel_response = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::ControlProjectChild {
            project_id,
            manager_session_id: root,
            task_id: manager_task.task_id,
            action: ProjectChildControlAction::Cancel,
        },
    )));
    assert_eq!(
        cancel_response.result.unwrap_err().code,
        ErrorCode::RecoveryRequired
    );
    assert_eq!(
        persistence
            .list_pending_project_cancellation_cascades()
            .unwrap()
            .len(),
        1
    );

    let updates = backend
        .journal()
        .unwrap()
        .events_since(None, sequence_before_cancel)
        .into_iter()
        .filter_map(|event| match event.event {
            ServerEvent::ProjectTaskUpdated { task }
                if task.status == loom_core::DelegatedTaskStatus::Cancelled
                    && (task.task_id == manager_task.task_id
                        || task.task_id == grandchild_task.task_id) =>
            {
                Some(task.task_id)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(updates.is_empty());
    assert_eq!(
        persistence
            .load_delegated_task(grandchild_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Queued
    );

    drop(connection);
    backend.shutdown().unwrap();
    manager_stream.join().unwrap();
    sibling_stream.join().unwrap();
    drop(backend);
    drop(model);

    let reopened =
        InProcessBackend::with_provider_registry_persistent(provider_registry(), &persistence_path)
            .unwrap();
    let reopened_persistence = reopened.persistence.as_ref().unwrap();
    assert!(
        reopened_persistence
            .list_pending_project_cancellation_cascades()
            .unwrap()
            .is_empty(),
        "startup must finish the persisted cancellation before returning"
    );
    let recovered_cancel_updates = reopened
        .journal()
        .unwrap()
        .events_since(None, sequence_before_cancel)
        .into_iter()
        .filter_map(|event| match event.event {
            ServerEvent::ProjectTaskUpdated { task }
                if task.status == loom_core::DelegatedTaskStatus::Cancelled
                    && (task.task_id == manager_task.task_id
                        || task.task_id == grandchild_task.task_id) =>
            {
                Some(task.task_id)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        recovered_cancel_updates,
        vec![grandchild_task.task_id, manager_task.task_id],
        "startup should replay the captured cascade in deepest-first order"
    );
    let recovered = reopened_persistence
        .load_project_snapshot(project_id)
        .unwrap()
        .unwrap();
    assert_eq!(recovered.agents.len(), 4);
    assert_eq!(recovered.tasks.len(), 3);
    for task_id in [manager_task.task_id, grandchild_task.task_id] {
        assert_eq!(
            reopened_persistence
                .load_delegated_task(task_id)
                .unwrap()
                .unwrap()
                .status,
            loom_core::DelegatedTaskStatus::Cancelled
        );
    }
    let recovered_manager_run = reopened_persistence
        .load_latest_run_summary_for_session(manager_session.session_id)
        .unwrap()
        .unwrap()
        .snapshot;
    assert_eq!(recovered_manager_run.id, manager_run_id);
    assert_eq!(recovered_manager_run.state, AgentRunState::Cancelled);
    assert!(
        reopened_persistence
            .load_latest_run_summary_for_session(grandchild_task.target_session_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        reopened_persistence
            .load_delegated_task(sibling_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Blocked
    );
    drop(reopened);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn delegated_children_require_durable_storage_on_ephemeral_backends() {
    let backend = InProcessBackend::new();
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
            if !result.capabilities.contains(Capability::CreateProjectChild)
                && !result.capabilities.contains(Capability::SendProjectAgentMessage)
    ));
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Ephemeral project".into(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Manager".into(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    assert!(
        !connection
            .project_delegation_enabled_for_session(root)
            .unwrap(),
        "delegation tools should be unavailable without durable storage"
    );
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}
