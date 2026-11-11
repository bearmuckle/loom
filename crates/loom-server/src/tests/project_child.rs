//! In-process tests: project child.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn delegated_model_selector_size_matches_durable_storage() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    for (model_id, expected_code) in [
        (" ".to_owned(), ErrorCode::InvalidRequest),
        (
            "m".repeat(loom_persistence::MAX_MODEL_ID_BYTES + 1),
            ErrorCode::InvalidRequest,
        ),
        (
            "m".repeat(loom_persistence::MAX_MODEL_ID_BYTES),
            ErrorCode::UnsupportedCapability,
        ),
    ] {
        let error = connection
            .create_project_child(
                RequestId::new(),
                AgentSessionId::new(),
                "Child".to_owned(),
                loom_core::DelegatedTaskSpec {
                    intent: "test model size".to_owned(),
                    model_id,
                    context_references: Vec::new(),
                    dependencies: Vec::new(),
                    code_change: false,
                    permissions: Default::default(),
                    scope: None,
                },
            )
            .unwrap_err();
        // A selector at the bound passes size validation and reaches the
        // ephemeral-backend storage requirement instead.
        assert_eq!(error.code, expected_code);
    }
}

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
            reuse_local: false,
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
            activity_detail: loom_protocol::ActivityDetail::Full,
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
            activity_detail: loom_protocol::ActivityDetail::Full,
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
        RunRequest::GetAgentRunSnapshot {
            activity_detail: loom_protocol::ActivityDetail::Full,
            run_id,
        },
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
        goal: None,
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
        title_source: loom_core::SessionTitleSource::Auto,
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
                scope: None,
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

/// A child beyond the concurrency limit reports that it is queued, where it
/// waits, and the bound it waits on; cancelling it moves the children behind it
/// forward without touching the running child.
#[test]
fn project_queue_reports_position_and_cancel_moves_the_rest_up() {
    let (endpoint, request_seen) = slow_model_endpoint();
    let path = std::env::temp_dir().join(format!("loom-project-queue-{}.db", uuid::Uuid::new_v4()));
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
                name: "Project queue".into(),
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

    let create_child = |child_name: &str| match connection
        .create_project_child(
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
                scope: None,
            },
        )
        .unwrap()
    {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, child }) => {
            (*task, child)
        }
        response => panic!("unexpected child creation response: {response:?}"),
    };

    let first = create_child("worker-1");
    request_seen
        .recv_timeout(Duration::from_secs(3))
        .expect("first child should begin its provider request");
    let second = create_child("worker-2");
    assert_eq!(first.0.status, loom_core::DelegatedTaskStatus::Running);
    assert_eq!(second.0.status, loom_core::DelegatedTaskStatus::Queued);

    let extension = backend
        .project_agent_tools(
            root,
            ModelId::new("slow/model"),
            ProjectAgentToolGrants {
                delegation: true,
                inspection: true,
                child_control: true,
                ..ProjectAgentToolGrants::default()
            },
        )
        .unwrap()
        .expect("manager child tools");
    let tools = ToolExecutor::new_with_workspace(connection.session_filesystem(root).unwrap())
        .with_extension(extension);
    let listing = || {
        let listing = tools.execute(&ToolCall {
            id: ToolCallId::new(),
            name: "list_project_children".to_owned(),
            arguments: serde_json::json!({}),
        });
        assert!(listing.success, "{}", listing.output);
        serde_json::from_str::<serde_json::Value>(&listing.output).unwrap()
    };
    let entry = |listing: &serde_json::Value, task_id: &serde_json::Value| {
        listing["children"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| &entry["task_id"] == task_id)
            .unwrap_or_else(|| panic!("child {task_id} should be listed: {listing}"))
            .clone()
    };
    let child_tool = |name: &str, arguments: serde_json::Value| {
        let result = tools.execute(&ToolCall {
            id: ToolCallId::new(),
            name: name.to_owned(),
            arguments,
        });
        assert!(result.success, "{}", result.output);
        serde_json::from_str::<serde_json::Value>(&result.output).unwrap()
    };

    // A delegation beyond the limit reports that the child is queued, its place
    // in the waiting order, and the effective limit.
    let third = child_tool(
        "delegate_project_task",
        serde_json::json!({
            "child_name": "worker-3",
            "intent": "Review worker-3",
            "model_id": "slow/model",
        }),
    );
    assert_eq!(third["status"], "queued");
    assert_eq!(third["queue_position"], serde_json::json!(2));
    assert_eq!(
        third["project_agent_concurrency_limit"],
        serde_json::json!(1)
    );
    assert!(
        third["note"]
            .as_str()
            .unwrap_or_default()
            .contains("queued"),
        "a queued delegation says it has not started: {third}"
    );
    let third_id = third["task_id"].clone();
    let second_id = serde_json::json!(second.0.task_id);
    let first_id = serde_json::json!(first.0.task_id);

    let listed = listing();
    assert_eq!(
        listed["project_agent_concurrency_limit"],
        serde_json::json!(1),
        "the listing names the effective limit: {listed}"
    );
    let running = entry(&listed, &first_id);
    assert_eq!(running["task_status"], "running");
    assert_eq!(running["outcome"], "running");
    assert!(running["queue_position"].is_null(), "{running}");
    assert!(running["queued_since"].is_null(), "{running}");

    let queued_second = entry(&listed, &second_id);
    assert_eq!(queued_second["task_status"], "queued");
    assert_eq!(queued_second["outcome"], "queued");
    assert_eq!(queued_second["queue_position"], serde_json::json!(1));
    assert!(
        queued_second["queued_since"].as_u64().is_some(),
        "a queued child reports when it joined the queue: {queued_second}"
    );
    assert!(
        queued_second["paused_reason"].is_null(),
        "a queued child is not paused: {queued_second}"
    );
    assert_eq!(
        entry(&listed, &third_id)["queue_position"],
        serde_json::json!(2)
    );

    // A control action that cannot start a queued child reports where it waits
    // instead of doing nothing silently.
    let continued = child_tool(
        "control_project_child",
        serde_json::json!({"task_id": third_id.clone(), "action": "continue"}),
    );
    assert_eq!(continued["status"], "queued");
    assert_eq!(continued["queue_position"], serde_json::json!(2));
    assert_eq!(
        continued["project_agent_concurrency_limit"],
        serde_json::json!(1)
    );
    assert!(
        continued["note"]
            .as_str()
            .unwrap_or_default()
            .contains("queued at position 2"),
        "a control action on a queued child names its place: {continued}"
    );

    // Cancelling the first queued child removes it before its run starts and
    // leaves the child behind it first in line.
    let cancelled = child_tool(
        "control_project_child",
        serde_json::json!({"task_id": second_id.clone(), "action": "cancel"}),
    );
    assert_eq!(cancelled["status"], "cancelled");

    let listed = listing();
    let cancelled_second = entry(&listed, &second_id);
    assert_eq!(cancelled_second["task_status"], "cancelled");
    assert_eq!(cancelled_second["outcome"], "terminal");
    assert!(cancelled_second["queue_position"].is_null());
    let moved_third = entry(&listed, &third_id);
    assert_eq!(moved_third["task_status"], "queued");
    assert_eq!(moved_third["queue_position"], serde_json::json!(1));
    let still_running = entry(&listed, &first_id);
    assert_eq!(still_running["task_status"], "running");
    assert!(still_running["queue_position"].is_null());

    backend.shutdown().unwrap();
    drop(tools);
    drop(connection);
    drop(backend);
    let _ = fs::remove_dir_all(path.with_extension("session-roots"));
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
        goal: None,
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
        title_source: loom_core::SessionTitleSource::Auto,
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
                onto: None,
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
        goal: None,
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
        title_source: loom_core::SessionTitleSource::Auto,
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
                onto: None,
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
            onto: None,
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
    // A retry does not fit a run that is waiting for an approval, so it is
    // refused with the actual reason instead of silently doing nothing.
    let retry_refusal = retry_while_awaiting_approval.result.unwrap_err();
    assert_eq!(retry_refusal.code, ErrorCode::InvalidState);
    assert!(
        retry_refusal.message.contains("apply_patch") && retry_refusal.message.contains("approval"),
        "the refusal names the approval the child waits on: {}",
        retry_refusal.message
    );
    let approval_events = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            activity_detail: loom_protocol::ActivityDetail::Full,
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
                        onto: None,
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
        goal: None,
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
        title_source: loom_core::SessionTitleSource::Auto,
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
            onto: None,
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
                onto: None,
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
                scope: None,
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
                scope: None,
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
            onto: None,
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

/// The manager's listing, waiting, and control tools for one code child fixture.
fn child_reporting_tools(fixture: &ProjectCodeChildFixture) -> ToolExecutor {
    let extension = fixture
        .backend
        .project_agent_tools(
            fixture.root_id,
            ModelId::new("deterministic/demo"),
            ProjectAgentToolGrants {
                delegation: true,
                inspection: true,
                child_control: true,
                worktree: true,
                review: true,
                ..ProjectAgentToolGrants::default()
            },
        )
        .unwrap()
        .expect("manager child reporting tools");
    ToolExecutor::new_with_workspace(
        fixture
            .connection
            .session_filesystem(fixture.root_id)
            .unwrap(),
    )
    .with_extension(extension)
}

/// Runs one `list_project_children` call and returns the fixture child's entry.
fn listed_child(tools: &ToolExecutor, task_id: loom_core::TaskId) -> serde_json::Value {
    let listing = tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "list_project_children".to_owned(),
        arguments: serde_json::json!({}),
    });
    assert!(listing.success, "{}", listing.output);
    let listing: serde_json::Value = serde_json::from_str(&listing.output).unwrap();
    listing["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["task_id"] == serde_json::json!(task_id))
        .unwrap_or_else(|| panic!("the child should be listed: {listing}"))
        .clone()
}

/// Runs one `control_project_child` action through the manager's own tools.
fn control_child_tool(
    tools: &ToolExecutor,
    task_id: loom_core::TaskId,
    action: &str,
) -> ToolResult {
    tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "control_project_child".to_owned(),
        arguments: serde_json::json!({"task_id": task_id, "action": action}),
    })
}

/// Runs one `wait_for_project_children` call for the fixture child.
fn wait_for_child(tools: &ToolExecutor, task_id: loom_core::TaskId) -> ToolResult {
    tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "wait_for_project_children".to_owned(),
        arguments: serde_json::json!({"task_ids": [task_id]}),
    })
}

/// A stopped child reports the reason it stopped, so the manager does not have
/// to guess which control action applies.
#[test]
fn listing_a_stopped_child_reports_why_it_stopped() {
    // An approval wait names the tool and a digest of what it wants to run.
    let fixture = project_code_child_fixture();
    let tools = child_reporting_tools(&fixture);
    let mut run = ParkedChildRun::new(AgentRunState::AwaitingApproval);
    run.pending_approval = Some(ToolCall {
        id: ToolCallId::new(),
        name: "apply_patch".to_owned(),
        arguments: serde_json::json!({"path": "README.md"}),
    });
    fixture.persist_child_run(run);
    let child = listed_child(&tools, fixture.task_id);
    assert_eq!(child["paused_reason"]["reason"], "awaiting_approval");
    assert_eq!(child["paused_reason"]["tool"], "apply_patch");
    assert_eq!(
        child["paused_reason"]["input_digest"]
            .as_str()
            .map(str::len),
        Some(16),
        "the wait carries a digest instead of the arguments: {child}"
    );
    assert_eq!(child["outcome"], "needs_attention");
    fixture.shutdown();

    // A provider failure is told apart from the approval wait above.
    let fixture = project_code_child_fixture();
    let tools = child_reporting_tools(&fixture);
    let mut run = ParkedChildRun::new(AgentRunState::Failed);
    run.summary = Some("Agent failed: scripted-project is unavailable (HTTP 503)".to_owned());
    run.attempts = 2;
    fixture.persist_child_run(run);
    let child = listed_child(&tools, fixture.task_id);
    assert_eq!(child["paused_reason"]["reason"], "provider_error");
    assert_eq!(child["paused_reason"]["kind"], "provider_unavailable");
    assert_eq!(child["paused_reason"]["attempts"], 2);
    assert!(
        child["paused_reason"]["last_status"]
            .as_str()
            .is_some_and(|status| status.contains("HTTP 503")),
        "a provider outage is visible without reading logs: {child}"
    );
    assert_eq!(child["outcome"], "needs_attention");
    fixture.shutdown();

    // A limit stop reports the budget it reached.
    let fixture = project_code_child_fixture();
    let tools = child_reporting_tools(&fixture);
    let mut run = ParkedChildRun::new(AgentRunState::Failed);
    run.summary = Some("Agent failed: agent session limit reached".to_owned());
    run.usage = UsageSnapshot {
        input_tokens: 10,
        output_tokens: 2,
        tool_calls: 4,
        ..Default::default()
    };
    fixture.persist_child_run(run);
    let child = listed_child(&tools, fixture.task_id);
    assert_eq!(child["paused_reason"]["reason"], "budget_exhausted");
    assert_eq!(child["paused_reason"]["steps"], 4);
    assert_eq!(child["paused_reason"]["tokens"], 12);
    fixture.shutdown();

    // A failed step names the tool that failed.
    let fixture = project_code_child_fixture();
    let tools = child_reporting_tools(&fixture);
    let mut run = ParkedChildRun::new(AgentRunState::Failed);
    run.summary = Some(
        "Agent failed: the model sent undecodable arguments for tool 'run_command' 3 times"
            .to_owned(),
    );
    run.last_failed_call = Some(ToolCall {
        id: ToolCallId::new(),
        name: "run_command".to_owned(),
        arguments: serde_json::json!({}),
    });
    fixture.persist_child_run(run);
    let child = listed_child(&tools, fixture.task_id);
    assert_eq!(child["paused_reason"]["reason"], "step_failed");
    assert_eq!(child["paused_reason"]["tool"], "run_command");
    assert_eq!(
        child["paused_reason"]["error_kind"],
        "undecodable_tool_arguments"
    );
    fixture.shutdown();

    // A question names the interaction a person has to answer.
    let fixture = project_code_child_fixture();
    let tools = child_reporting_tools(&fixture);
    let mut run = ParkedChildRun::new(AgentRunState::NeedsInput);
    run.pending_input = Some("Which option should I use?".to_owned());
    run.pending_question = true;
    fixture.persist_child_run(run);
    let child = listed_child(&tools, fixture.task_id);
    assert_eq!(child["paused_reason"]["reason"], "awaiting_input");
    assert!(
        child["paused_reason"]["question_id"]
            .as_str()
            .is_some_and(|question| !question.is_empty()),
        "the wait names the question a person has to answer: {child}"
    );
    fixture.shutdown();

    // A stopped child with nothing to explain it reports `unknown` rather than
    // nothing at all.
    let fixture = project_code_child_fixture();
    let tools = child_reporting_tools(&fixture);
    fixture.persist_child_run(ParkedChildRun::new(AgentRunState::Paused));
    let child = listed_child(&tools, fixture.task_id);
    assert_eq!(child["paused_reason"]["reason"], "unknown");
    assert_eq!(child["outcome"], "needs_attention");
    fixture.shutdown();

    // A finished child reports no reason and reads as terminal, so a manager is
    // not pushed into acting on work that is done.
    let fixture = project_code_child_fixture();
    let tools = child_reporting_tools(&fixture);
    fixture.persist_child_run(ParkedChildRun::new(AgentRunState::Completed));
    complete_project_code_child(&fixture);
    let child = listed_child(&tools, fixture.task_id);
    assert_eq!(child["paused_reason"], serde_json::Value::Null);
    assert_eq!(child["outcome"], "terminal");
    fixture.shutdown();
}

/// A wait separates a child that needs attention from one that is terminal.
#[test]
fn waiting_for_children_distinguishes_attention_from_terminal() {
    // The task finished, but its run failed: the wait is ready and the child
    // still needs the manager's attention.
    let fixture = project_code_child_fixture();
    let tools = child_reporting_tools(&fixture);
    let mut run = ParkedChildRun::new(AgentRunState::Failed);
    run.summary =
        Some("Agent failed: scripted-project is rate limited or timed out (HTTP 429)".to_owned());
    fixture.persist_child_run(run);
    complete_project_code_child(&fixture);
    let waited = wait_for_child(&tools, fixture.task_id);
    assert!(waited.success, "{}", waited.output);
    let waited: serde_json::Value = serde_json::from_str(&waited.output).unwrap();
    assert_eq!(waited["return_ready"], true);
    assert_eq!(waited["outcome"], "needs_attention");
    assert_eq!(waited["children"][0]["outcome"], "needs_attention");
    assert_eq!(
        waited["children"][0]["paused_reason"]["reason"],
        "provider_error"
    );
    assert!(
        waited["needs_attention"]
            .as_array()
            .is_some_and(|children| children.iter().any(|child| child
                .as_str()
                .is_some_and(|child| child.contains("provider_error")))),
        "the wait names the child and its reason: {waited}"
    );
    fixture.shutdown();

    // A child that produced its result is terminal.
    let fixture = project_code_child_fixture();
    let tools = child_reporting_tools(&fixture);
    fixture.persist_child_run(ParkedChildRun::new(AgentRunState::Completed));
    complete_project_code_child(&fixture);
    let waited = wait_for_child(&tools, fixture.task_id);
    assert!(waited.success, "{}", waited.output);
    let waited: serde_json::Value = serde_json::from_str(&waited.output).unwrap();
    assert_eq!(waited["return_ready"], true);
    assert_eq!(waited["outcome"], "terminal");
    assert_eq!(waited["children"][0]["outcome"], "terminal");
    assert_eq!(
        waited["children"][0]["paused_reason"],
        serde_json::Value::Null
    );
    assert_eq!(waited["needs_attention"], serde_json::json!([]));
    fixture.shutdown();
}

/// A terminal wait collects only the children that produced their final state,
/// and names the parked ones separately so neither is lost.
#[test]
fn a_terminal_wait_separates_parked_children_from_terminal_ones() {
    let fixture = project_code_child_fixture();
    let tools = child_reporting_tools(&fixture);
    let persistence = fixture.backend.persistence.as_ref().unwrap();
    // The fixture child finished, and a sibling is parked waiting for approval.
    fixture.persist_child_run(ParkedChildRun::new(AgentRunState::Completed));
    complete_project_code_child(&fixture);
    // The sibling is created through persistence so nothing schedules it: it is
    // parked waiting for an approval, which is the state the wait must report.
    let workspace_id = fixture
        .backend
        .sessions()
        .unwrap()
        .get(fixture.root_id)
        .unwrap()
        .workspace_id;
    let parked_child_id = AgentSessionId::new();
    let parked_task_id = loom_core::TaskId::new();
    let created_at = Timestamp::now();
    let parked_snapshot = AgentSessionSnapshot {
        id: parked_child_id,
        workspace_id,
        name: "Parked child".to_owned(),
        goal: None,
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
        title_source: loom_core::SessionTitleSource::Auto,
    };
    let parked_task = loom_core::DelegatedTaskRecord {
        task_id: parked_task_id,
        project_id: fixture.project_id,
        requester_session_id: fixture.root_id,
        target_session_id: parked_child_id,
        child_name: "Parked child".to_owned(),
        intent: "Wait for an approval".to_owned(),
        model_id: "deterministic/demo".to_owned(),
        context_references: Vec::new(),
        dependencies: Vec::new(),
        code_change: false,
        permissions: loom_core::ProjectAgentPermissions::default(),
        status: loom_core::DelegatedTaskStatus::Queued,
        created_at,
        updated_at: created_at,
    };
    persistence
        .create_project_child(
            RequestId::new(),
            &parked_snapshot,
            fixture.backend.sessions().unwrap().next_sequence().next(),
            &parked_task,
        )
        .unwrap();
    let (_, created_event) = fixture
        .backend
        .sessions()
        .unwrap()
        .create_in_workspace_with_id(workspace_id, parked_child_id, "Parked child".to_owned())
        .unwrap();
    fixture
        .backend
        .journal()
        .unwrap()
        .append_session(created_event);
    assert!(
        persistence
            .update_delegated_task_status(
                parked_task_id,
                loom_core::DelegatedTaskStatus::Blocked,
                Timestamp::now(),
            )
            .unwrap()
    );
    let wait = |arguments: serde_json::Value| {
        let waited = tools.execute(&ToolCall {
            id: ToolCallId::new(),
            name: "wait_for_project_children".to_owned(),
            arguments,
        });
        assert!(waited.success, "{}", waited.output);
        let waited: serde_json::Value = serde_json::from_str(&waited.output).unwrap();
        waited
    };
    let task_ids = serde_json::json!([fixture.task_id, parked_task_id]);

    // The default wait still collects everything that stopped.
    let ready = wait(serde_json::json!({"task_ids": task_ids}));
    assert_eq!(ready["until"], "ready");
    assert_eq!(
        ready["children"].as_array().map(|children| children.len()),
        Some(2)
    );
    assert_eq!(ready["still_paused"], serde_json::json!([]));

    // A terminal wait returns only the finished child and names the parked one.
    let terminal = wait(serde_json::json!({"task_ids": task_ids, "until": "terminal"}));
    assert_eq!(terminal["until"], "terminal");
    assert_eq!(terminal["return_ready"], true);
    let children = terminal["children"].as_array().unwrap();
    assert_eq!(children.len(), 1, "{terminal}");
    assert_eq!(children[0]["task_id"], serde_json::json!(fixture.task_id));
    assert_eq!(children[0]["outcome"], "terminal");
    let still_paused = terminal["still_paused"].as_array().unwrap();
    assert_eq!(still_paused.len(), 1, "{terminal}");
    assert_eq!(
        still_paused[0]["task_id"],
        serde_json::json!(parked_task_id)
    );
    assert_eq!(still_paused[0]["outcome"], "needs_attention");
    assert!(
        still_paused[0]["paused_reason"]["reason"]
            .as_str()
            .is_some(),
        "the parked child keeps the reason it stopped: {terminal}"
    );
    assert!(
        terminal["note"]
            .as_str()
            .is_some_and(|note| note.contains("terminal children only")),
        "the result says what it collected: {terminal}"
    );
    fixture.shutdown();
}

/// A code child's committed result is listed with its commit subject, patch ids
/// and changed paths, so a manager can tell whether the work is already landed
/// without running git in its own checkout.
#[test]
fn listing_a_committed_code_child_reports_its_commit_identity() {
    let fixture = project_code_child_fixture();
    let tools = child_reporting_tools(&fixture);
    commit_file(
        &fixture.child_checkout,
        "src/result.txt",
        "result\n",
        "feat: add the result",
    );
    let reviewed = review_project_code_child(&fixture);
    complete_project_code_child(&fixture);
    let child = listed_child(&tools, fixture.task_id);
    assert_eq!(child["result_revision"], serde_json::json!(reviewed));
    assert_eq!(
        child["base_revision"],
        serde_json::json!(fixture.worktree.base_revision)
    );
    assert_eq!(child["commit_subject"], "feat: add the result");
    let patch_ids = child["patch_ids"].as_array().unwrap();
    assert_eq!(patch_ids.len(), 1, "{child}");
    assert_eq!(child["patch_ids_total"], 1);
    assert!(
        patch_ids[0]
            .as_str()
            .is_some_and(|patch_id| patch_id.len() == 16),
        "a patch id is reported in the form a review uses: {child}"
    );
    assert_eq!(
        child["changed_paths"],
        serde_json::json!(["src/result.txt"])
    );
    assert_eq!(child["changed_paths_total"], 1);
    assert_eq!(child["commit_summary_truncated"], false);
    assert_eq!(child["rebase"], serde_json::Value::Null);
    fixture.shutdown();
}

/// A change wider than the listing's bound is cut short, and the entry says so
/// with the exact total instead of dropping paths silently.
#[test]
fn listing_a_wide_change_marks_its_truncated_path_list() {
    let fixture = project_code_child_fixture();
    let tools = child_reporting_tools(&fixture);
    fs::create_dir_all(fixture.child_checkout.join("bulk")).unwrap();
    for index in 0..21 {
        fs::write(
            fixture
                .child_checkout
                .join(format!("bulk/file-{index:02}.txt")),
            format!("{index}\n"),
        )
        .unwrap();
    }
    git_in(&fixture.child_checkout, &["add", "--", "bulk"]);
    git_in(
        &fixture.child_checkout,
        &["commit", "-qm", "feat: add the bulk files"],
    );
    review_project_code_child(&fixture);
    complete_project_code_child(&fixture);
    let child = listed_child(&tools, fixture.task_id);
    assert_eq!(child["commit_summary_truncated"], true);
    assert_eq!(child["changed_paths_total"], 21);
    assert_eq!(
        child["changed_paths"].as_array().map(|paths| paths.len()),
        Some(20),
        "the path list stops at its bound: {child}"
    );
    fixture.shutdown();
}

/// A delegation can pin the revision its checkout is created from, and a base
/// that is not a commit of the parent repository is refused.
#[test]
fn a_delegation_pins_its_base_and_refuses_an_unknown_one() {
    let fixture = project_code_child_fixture();
    let tools = child_reporting_tools(&fixture);
    let base = fixture.worktree.base_revision.clone();
    // The parent keeps committing after the delegation names its base.
    commit_file(
        fixture.parent_git.root(),
        "parent.txt",
        "parent\n",
        "chore: move the parent on",
    );
    let parent_head = fixture.parent_git.status().unwrap().head.unwrap();
    assert_ne!(
        parent_head, base,
        "the parent advanced past the pinned base"
    );
    let persisted = fixture.backend.persistence.as_ref().unwrap();

    // The child's checkout starts at the pinned revision, not the parent head.
    let created = fixture
        .connection
        .create_project_child_from_base(
            RequestId::new(),
            fixture.root_id,
            "Pinned child".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Change one file".to_owned(),
                model_id: "deterministic/demo".to_owned(),
                context_references: Vec::new(),
                dependencies: Vec::new(),
                code_change: true,
                permissions: Default::default(),
                scope: None,
            },
            Some(&base),
        )
        .expect("a pinned base creates the child checkout");
    let created = match created {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, .. }) => *task,
        other => panic!("unexpected child creation response: {other:?}"),
    };
    let worktree = persisted
        .load_project_worktree_by_task(created.task_id)
        .unwrap()
        .expect("the pinned child has a durable checkout");
    assert_eq!(worktree.base_revision, base);
    persisted
        .update_delegated_task_status(
            created.task_id,
            loom_core::DelegatedTaskStatus::Cancelled,
            Timestamp::now(),
        )
        .unwrap();

    // An unknown revision is refused before anything is persisted, and the tool
    // reports that refusal.
    let refused = tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "delegate_project_code_task".to_owned(),
        arguments: serde_json::json!({
            "child_name": "Unknown base",
            "intent": "Change one file",
            "base": "0123456789abcdef0123456789abcdef01234567",
        }),
    });
    assert!(!refused.success, "{}", refused.output);
    assert!(
        refused
            .output
            .contains("is not a commit of the parent repository"),
        "the refusal names what the base has to be: {}",
        refused.output
    );
    fixture.shutdown();
}

/// A replay asked for by a delegation is reported honestly: the result says one
/// was requested, that none has happened yet, and where a replay Loom performs
/// is recorded. A read-only delegation refuses both arguments.
#[test]
fn a_requested_replay_is_reported_and_refused_on_a_read_only_delegation() {
    let fixture = project_code_child_fixture();
    let tools = child_reporting_tools(&fixture);
    let delegated = tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "delegate_project_code_task".to_owned(),
        arguments: serde_json::json!({
            "child_name": "Replaying child",
            "intent": "Change one file",
            "rebase_before_report": true,
        }),
    });
    assert!(delegated.success, "{}", delegated.output);
    let delegated: serde_json::Value = serde_json::from_str(&delegated.output).unwrap();
    assert_eq!(delegated["rebase_before_report"]["requested"], true);
    assert_eq!(delegated["rebase_before_report"]["happened"], false);
    assert!(
        delegated["rebase_before_report"]["note"]
            .as_str()
            .is_some_and(|note| note.contains("rebase")),
        "the result says where a replay is reported: {delegated}"
    );
    assert!(
        delegated["intent"]
            .as_str()
            .is_some_and(|intent| intent.contains("replay your own commits")),
        "the child is told to replay before it reports: {delegated}"
    );
    let task_id: loom_core::TaskId = serde_json::from_value(delegated["task_id"].clone()).unwrap();
    fixture
        .backend
        .persistence
        .as_ref()
        .unwrap()
        .update_delegated_task_status(
            task_id,
            loom_core::DelegatedTaskStatus::Cancelled,
            Timestamp::now(),
        )
        .unwrap();

    // The same arguments are refused on a read-only delegation.
    for arguments in [
        serde_json::json!({
            "child_name": "Read only",
            "intent": "Read one file",
            "rebase_before_report": true,
        }),
        serde_json::json!({
            "child_name": "Read only",
            "intent": "Read one file",
            "base": "current",
        }),
    ] {
        let refused = tools.execute(&ToolCall {
            id: ToolCallId::new(),
            name: "delegate_project_task".to_owned(),
            arguments: arguments.clone(),
        });
        assert!(!refused.success, "{}: {}", arguments, refused.output);
        assert!(
            refused.output.contains("code task"),
            "the refusal says where the argument applies: {}",
            refused.output
        );
    }
    fixture.shutdown();
}

/// Reviewing a paused child returns the uncommitted work it left behind, which
/// is the cheapest way to triage a stalled child.
#[test]
fn reviewing_a_paused_child_returns_its_uncommitted_work() {
    let fixture = project_code_child_fixture();
    let tools = child_reporting_tools(&fixture);
    fs::write(fixture.child_checkout.join("stalled.txt"), "half written\n").unwrap();
    let mut run = ParkedChildRun::new(AgentRunState::AwaitingApproval);
    run.pending_approval = Some(ToolCall {
        id: ToolCallId::new(),
        name: "apply_patch".to_owned(),
        arguments: serde_json::json!({"path": "stalled.txt"}),
    });
    fixture.persist_child_run(run);
    fixture
        .backend
        .persistence
        .as_ref()
        .unwrap()
        .update_delegated_task_status(
            fixture.task_id,
            loom_core::DelegatedTaskStatus::Blocked,
            Timestamp::now(),
        )
        .unwrap();

    let review = tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "review_project_child".to_owned(),
        arguments: serde_json::json!({"task_id": fixture.task_id, "mode": "diff"}),
    });
    assert!(review.success, "{}", review.output);
    let review: serde_json::Value = serde_json::from_str(&review.output).unwrap();
    assert_eq!(review["type"], "project_child_review");
    assert_eq!(review["data"]["mode"], "diff");
    assert_eq!(review["data"]["run_state"], "awaiting_approval");
    let files = review["data"]["files"].as_array().unwrap();
    let stalled = files
        .iter()
        .find(|file| file["path"] == "stalled.txt")
        .unwrap_or_else(|| panic!("the uncommitted path is reviewed: {review}"));
    assert!(
        stalled["patch"]
            .as_str()
            .is_some_and(|patch| patch.contains("half written")),
        "the review carries the uncommitted content: {review}"
    );
    // A stat review names the same path without its patch.
    let stat = tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "review_project_child".to_owned(),
        arguments: serde_json::json!({"task_id": fixture.task_id}),
    });
    assert!(stat.success, "{}", stat.output);
    let stat: serde_json::Value = serde_json::from_str(&stat.output).unwrap();
    assert!(
        stat["data"]["files"]
            .as_array()
            .is_some_and(|files| files.iter().any(|file| file["path"] == "stalled.txt")),
        "a stat review names the uncommitted path: {stat}"
    );
    fixture.shutdown();
}

/// A result that states which optional dependencies its checks reached is
/// reported as stated, and one that stays silent is reported as unstated rather
/// than as `no`.
#[test]
fn a_result_reports_which_dependencies_its_checks_ran_against() {
    fn report(fixture: &ProjectCodeChildFixture, body: &str) {
        fixture
            .backend
            .accept_project_agent_message(
                RequestId::new(),
                fixture.worktree.child_session_id,
                true,
                false,
                loom_core::AgentMessageDraft {
                    project_id: fixture.project_id,
                    task_id: Some(fixture.task_id),
                    sender_session_id: fixture.worktree.child_session_id,
                    target_session_id: fixture.root_id,
                    kind: loom_core::AgentMessageKind::Result,
                    body: body.to_owned(),
                },
            )
            .expect("a finished child reports its result");
    }

    // A silent result is reported as unstated, and the listing says so.
    let fixture = project_code_child_fixture();
    let tools = child_reporting_tools(&fixture);
    fixture.persist_child_run(ParkedChildRun::new(AgentRunState::Completed));
    complete_project_code_child(&fixture);
    report(&fixture, "All green.");
    let child = listed_child(&tools, fixture.task_id);
    assert_eq!(child["checks_ran_against"]["live_db"], "not_stated");
    assert_eq!(child["checks_ran_against"]["stated"], false);
    let listing = tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "list_project_children".to_owned(),
        arguments: serde_json::json!({}),
    });
    let listing: serde_json::Value = serde_json::from_str(&listing.output).unwrap();
    assert!(
        listing["note"]
            .as_str()
            .is_some_and(|note| note.contains("did not state which optional dependencies")),
        "the listing names the child whose result did not say: {listing}"
    );
    fixture.shutdown();

    // A stated result is read back field by field, and the wait reports it too.
    let fixture = project_code_child_fixture();
    let tools = child_reporting_tools(&fixture);
    fixture.persist_child_run(ParkedChildRun::new(AgentRunState::Completed));
    complete_project_code_child(&fixture);
    report(
        &fixture,
        "Checks passed.\nlive-db: yes\nkeychain: no\nnetwork: no\n",
    );
    let child = listed_child(&tools, fixture.task_id);
    assert_eq!(child["checks_ran_against"]["live_db"], "yes");
    assert_eq!(child["checks_ran_against"]["keychain"], "no");
    assert_eq!(child["checks_ran_against"]["network"], "no");
    assert_eq!(child["checks_ran_against"]["stated"], true);
    let waited = wait_for_child(&tools, fixture.task_id);
    assert!(waited.success, "{}", waited.output);
    let waited: serde_json::Value = serde_json::from_str(&waited.output).unwrap();
    assert_eq!(
        waited["children"][0]["checks_ran_against"]["live_db"],
        "yes"
    );
    assert_eq!(waited["children"][0]["checks_ran_against"]["stated"], true);
    fixture.shutdown();
}

/// A control action that does not fit the child names the actual reason.
#[test]
fn a_control_action_that_does_not_fit_names_the_reason() {
    // A retry does not fit a run waiting for an approval the parent cannot
    // grant, and neither does a resume.
    let fixture = project_code_child_fixture();
    let tools = child_reporting_tools(&fixture);
    let mut run = ParkedChildRun::new(AgentRunState::AwaitingApproval);
    run.pending_approval = Some(ToolCall {
        id: ToolCallId::new(),
        name: "apply_patch".to_owned(),
        arguments: serde_json::json!({"path": "README.md"}),
    });
    fixture.persist_child_run(run);
    let retried = control_child_tool(&tools, fixture.task_id, "retry_failed_step");
    assert!(!retried.success, "{}", retried.output);
    assert!(
        retried.output.contains("apply_patch")
            && retried
                .output
                .contains("a parent cannot grant this approval")
            && retried.output.contains("a person has to resolve the wait"),
        "the refusal names the approval wait and who can resolve it: {}",
        retried.output
    );
    let continued = control_child_tool(&tools, fixture.task_id, "continue");
    assert!(!continued.success, "{}", continued.output);
    assert!(
        continued.output.contains("approval"),
        "a resume says the wait needs an approval: {}",
        continued.output
    );
    fixture.shutdown();

    // A resume does not fit a run that stopped at a failed tool step.
    let fixture = project_code_child_fixture();
    let tools = child_reporting_tools(&fixture);
    let mut run = ParkedChildRun::new(AgentRunState::Paused);
    run.summary =
        Some("Agent failed: tool execution was interrupted with an unknown outcome".to_owned());
    run.last_failed_call = Some(ToolCall {
        id: ToolCallId::new(),
        name: "run_command".to_owned(),
        arguments: serde_json::json!({}),
    });
    fixture.persist_child_run(run);
    let continued = control_child_tool(&tools, fixture.task_id, "continue");
    assert!(!continued.success, "{}", continued.output);
    assert!(
        continued.output.contains("run_command") && continued.output.contains("retry_failed_step"),
        "a resume names the failed step and the action that fits: {}",
        continued.output
    );
    fixture.shutdown();

    // A retry does not fit a run that reached a configured limit, because it
    // stops again at the same limit.
    let fixture = project_code_child_fixture();
    let tools = child_reporting_tools(&fixture);
    let mut run = ParkedChildRun::new(AgentRunState::Failed);
    run.summary = Some("Agent failed: agent session limit reached".to_owned());
    run.usage = UsageSnapshot {
        tool_calls: 2,
        ..Default::default()
    };
    fixture.persist_child_run(run);
    let retried = control_child_tool(&tools, fixture.task_id, "retry_failed_step");
    assert!(!retried.success, "{}", retried.output);
    assert!(
        retried.output.contains("limit"),
        "the refusal names the budget: {}",
        retried.output
    );
    fixture.shutdown();
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

/// A child state change reaches the parent's stream: the parent learns the
/// child's own agent record, followed by the delegated task update it already
/// received.
#[test]
fn a_child_state_change_journals_the_agent_record_to_its_parent() {
    let temp = std::env::temp_dir().join(format!("loom-child-state-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&temp).unwrap();
    let path = temp.join("state.sqlite");
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Child state".into(),
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

    // The durable records a delegated child owns: its session, hierarchy edge,
    // and task, created the way the manager tool creates them.
    let child_id = AgentSessionId::new();
    let created_at = Timestamp::now();
    let child = AgentSessionSnapshot {
        id: child_id,
        workspace_id: workspace.id,
        name: "Child".into(),
        goal: None,
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
        title_source: loom_core::SessionTitleSource::Auto,
    };
    let task = loom_core::DelegatedTaskRecord {
        task_id: loom_core::TaskId::new(),
        project_id: ProjectId::from_uuid(*root.id.as_uuid()),
        requester_session_id: root.id,
        target_session_id: child_id,
        child_name: child.name.clone(),
        intent: "Report the current state".into(),
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

    let (_, record) = backend
        .sessions()
        .unwrap()
        .transition(child_id, AgentSessionState::Completed)
        .unwrap();
    backend.journal().unwrap().append_session(record);
    // The project snapshot reads the durable session rows, which a checkpoint
    // writes; persist them so the child's state change is what the snapshot
    // reports.
    let sessions = backend.sessions().unwrap();
    let state = sessions.export_state();
    drop(sessions);
    backend
        .persistence
        .as_ref()
        .unwrap()
        .save_state(loom_persistence::DurableStateWrite {
            sessions: &state,
            workspaces: None,
            settings: None,
            workspace_configs: None,
            providers: None,
            usage: None,
            idempotency: None,
            run_summaries: None,
            run_runtime_configs: None,
            run_context_checkpoints: None,
            run_plans: None,
            run_messages: None,
            run_activities: None,
            filesystem_records: None,
            feed: None,
        })
        .unwrap();
    backend
        .update_project_task_for_session_state(child_id, AgentSessionState::Completed)
        .unwrap();

    let events = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            activity_detail: loom_protocol::ActivityDetail::Full,
            session_id: Some(root.id),
            workspace_id: None,
            after_sequence: None,
            stream_epoch: None,
        },
    )));
    let ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) =
        events.result.unwrap()
    else {
        panic!("unexpected parent events response");
    };
    let agent_update = events
        .iter()
        .position(|envelope| match &envelope.event {
            ServerEvent::ProjectAgentUpdated { agent } if agent.session_id == child_id => {
                assert_eq!(envelope.session_id, root.id);
                assert_eq!(agent.project_id, task.project_id);
                assert_eq!(agent.parent_session_id, Some(root.id));
                assert_eq!(agent.state, AgentSessionState::Completed);
                true
            }
            _ => false,
        })
        .expect("the parent's stream carries the child's agent record");
    let task_update = events
        .iter()
        .position(|envelope| {
            matches!(
                &envelope.event,
                ServerEvent::ProjectTaskUpdated { task: update }
                    if update.task_id == task.task_id
            )
        })
        .expect("the parent's stream carries the task update");
    assert!(
        agent_update < task_update,
        "the child's state change precedes the task update"
    );

    drop(connection);
    drop(backend);
    let _ = fs::remove_dir_all(&temp);
    let _ = fs::remove_file(path.with_extension("credentials.json"));
}

/// A workspace that opts in archives a completed delegated sub agent, while an
/// opted-out workspace keeps it visible. Code work is archived only after its
/// result is integrated, which the project integration tests cover.
#[test]
fn auto_archive_policy_archives_a_completed_child_agent() {
    let temp = std::env::temp_dir().join(format!("loom-auto-archive-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&temp).unwrap();
    let path = temp.join("state.sqlite");
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Auto archive".into(),
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

    // Creates a durable delegated child session and its task, like the manager
    // tool does, and returns the child's id.
    let delegate = |name: &str| {
        let child_id = AgentSessionId::new();
        let created_at = Timestamp::now();
        let child = AgentSessionSnapshot {
            id: child_id,
            workspace_id: workspace.id,
            name: name.to_owned(),
            goal: None,
            state: AgentSessionState::Idle,
            created_at,
            updated_at: created_at,
            title_source: loom_core::SessionTitleSource::Auto,
        };
        let task = loom_core::DelegatedTaskRecord {
            task_id: loom_core::TaskId::new(),
            project_id: ProjectId::from_uuid(*root.id.as_uuid()),
            requester_session_id: root.id,
            target_session_id: child_id,
            child_name: child.name.clone(),
            intent: "Report the current state".into(),
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
        child_id
    };

    // Finishing a child run transitions its session, then reconciles the task;
    // that reconciliation is what reclaims an opted-in completed agent.
    let complete = |child_id: AgentSessionId| {
        let (_, record) = backend
            .sessions()
            .unwrap()
            .transition(child_id, AgentSessionState::Completed)
            .unwrap();
        backend.journal().unwrap().append_session(record);
        backend
            .update_project_task_for_session_state(child_id, AgentSessionState::Completed)
            .unwrap();
    };

    backend
        .set_workspace_config(
            workspace.id,
            WorkspaceConfig {
                revision: 1,
                auto_archive_completed_agents: true,
                ..WorkspaceConfig::default()
            },
        )
        .unwrap();
    let archived_child = delegate("Archived child");
    // Build output the child's commands left behind is reclaimed with the
    // archive, so an auto-archived agent cannot leak its cache until the next
    // start sweeps it.
    let cache = backend
        .build_cache_for_session(archived_child)
        .expect("a durable backend owns a build cache");
    fs::create_dir_all(cache.join("debug")).unwrap();
    fs::write(cache.join("debug/artifact"), b"build output").unwrap();
    complete(archived_child);
    assert_eq!(
        backend
            .sessions()
            .unwrap()
            .get(archived_child)
            .unwrap()
            .state,
        AgentSessionState::Archived
    );
    assert!(
        !cache.exists(),
        "auto-archiving a completed child must reclaim its build cache"
    );
    assert_eq!(
        backend
            .persistence
            .as_ref()
            .unwrap()
            .load_delegated_task_for_target(archived_child)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Completed
    );

    backend
        .set_workspace_config(
            workspace.id,
            WorkspaceConfig {
                revision: 2,
                auto_archive_completed_agents: false,
                ..WorkspaceConfig::default()
            },
        )
        .unwrap();
    let visible_child = delegate("Visible child");
    complete(visible_child);
    assert_eq!(
        backend
            .sessions()
            .unwrap()
            .get(visible_child)
            .unwrap()
            .state,
        AgentSessionState::Completed,
        "an opted-out workspace keeps a finished child visible"
    );

    // Turning the switch on reaches the agents that finished while it was off:
    // the maintenance pass archives them without waiting for a new checkpoint.
    backend
        .set_workspace_config(
            workspace.id,
            WorkspaceConfig {
                revision: 3,
                auto_archive_completed_agents: true,
                ..WorkspaceConfig::default()
            },
        )
        .unwrap();
    assert_eq!(
        backend.reconcile_auto_archive_completed_children().unwrap(),
        vec![visible_child]
    );
    assert_eq!(
        backend
            .sessions()
            .unwrap()
            .get(visible_child)
            .unwrap()
            .state,
        AgentSessionState::Archived,
        "the reconcile pass archives a child that completed while opted out"
    );
    // A second pass has nothing left to do.
    assert!(
        backend
            .reconcile_auto_archive_completed_children()
            .unwrap()
            .is_empty()
    );

    drop(connection);
    drop(backend);
    let _ = fs::remove_dir_all(&temp);
    let _ = fs::remove_file(path.with_extension("credentials.json"));
}

/// A code child is reclaimed only after its result is integrated, so an
/// unreviewed checkout never disappears when the policy is on.
#[test]
fn auto_archive_waits_for_code_integration() {
    let temp =
        std::env::temp_dir().join(format!("loom-auto-archive-code-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&temp).unwrap();
    let path = temp.join("state.sqlite");
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Auto archive code".into(),
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
        name: "Code child".into(),
        goal: None,
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
        title_source: loom_core::SessionTitleSource::Auto,
    };
    let task = loom_core::DelegatedTaskRecord {
        task_id: loom_core::TaskId::new(),
        project_id: ProjectId::from_uuid(*root.id.as_uuid()),
        requester_session_id: root.id,
        target_session_id: child_id,
        child_name: child.name.clone(),
        intent: "Change the checkout".into(),
        model_id: "deterministic/demo".into(),
        context_references: vec![],
        dependencies: vec![],
        code_change: true,
        permissions: loom_core::ProjectAgentPermissions::default(),
        status: loom_core::DelegatedTaskStatus::Queued,
        created_at,
        updated_at: created_at,
    };
    let mut worktree = loom_core::ProjectWorktreeRecord {
        project_id: task.project_id,
        task_id: task.task_id,
        parent_session_id: root.id,
        child_session_id: child_id,
        parent_repository_id: loom_core::RepositoryId::new(),
        child_repository_id: loom_core::RepositoryId::new(),
        relative_path: "project-worktrees/child".into(),
        worktree_name: "loom-child".into(),
        branch_name: "loom/project-child".into(),
        base_revision: "0123456789abcdef0123456789abcdef01234567".into(),
        base_refresh: None,
        result_revision: None,
        integrated_revision: None,
        landed_by: None,
        landed_evidence: None,
        scope: None,
        scope_override: None,
        rebase: None,
        status: loom_core::ProjectWorktreeStatus::Creating,
        conflict_paths: vec![],
        error: None,
        cleanup_disposition: None,
        created_at,
        updated_at: created_at,
    };
    let persistence = backend.persistence.as_ref().unwrap();
    persistence
        .create_project_child_with_worktree(
            RequestId::new(),
            &child,
            backend.sessions().unwrap().next_sequence().next(),
            &task,
            &worktree,
        )
        .unwrap();
    let (_, created_event) = backend
        .sessions()
        .unwrap()
        .create_in_workspace_with_id(workspace.id, child_id, child.name.clone())
        .unwrap();
    backend.journal().unwrap().append_session(created_event);
    worktree.status = loom_core::ProjectWorktreeStatus::Ready;
    persistence.save_project_worktree(&worktree).unwrap();
    persistence
        .update_delegated_task_status(
            task.task_id,
            loom_core::DelegatedTaskStatus::Completed,
            Timestamp::now(),
        )
        .unwrap();
    let (_, record) = backend
        .sessions()
        .unwrap()
        .transition(child_id, AgentSessionState::Completed)
        .unwrap();
    backend.journal().unwrap().append_session(record);

    backend
        .set_workspace_config(
            workspace.id,
            WorkspaceConfig {
                revision: 1,
                auto_archive_completed_agents: true,
                ..WorkspaceConfig::default()
            },
        )
        .unwrap();

    // A ready but un-integrated result keeps the child visible.
    backend.auto_archive_completed_child(child_id).unwrap();
    assert_eq!(
        backend.sessions().unwrap().get(child_id).unwrap().state,
        AgentSessionState::Completed,
        "an un-integrated code result must stay visible"
    );
    // The maintenance pass applies the same guard, so it cannot reclaim it
    // either.
    assert!(
        backend
            .reconcile_auto_archive_completed_children()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        backend.sessions().unwrap().get(child_id).unwrap().state,
        AgentSessionState::Completed
    );

    // Once integrated, the policy reclaims it.
    worktree.status = loom_core::ProjectWorktreeStatus::Integrated;
    worktree.integrated_revision = Some("fedcba9876543210fedcba9876543210fedcba98".into());
    persistence.save_project_worktree(&worktree).unwrap();
    assert_eq!(
        backend.reconcile_auto_archive_completed_children().unwrap(),
        vec![child_id]
    );
    assert_eq!(
        backend.sessions().unwrap().get(child_id).unwrap().state,
        AgentSessionState::Archived
    );

    drop(connection);
    drop(backend);
    let _ = fs::remove_dir_all(&temp);
    let _ = fs::remove_file(path.with_extension("credentials.json"));
}
