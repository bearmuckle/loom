//! In-process tests: project manager.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn delegated_child_current_model_alias_resolves_to_manager_model() {
    let current_model = ModelId::new("provider/model");

    assert_eq!(
        delegated_child_model_id(None, &current_model),
        "provider/model"
    );
    assert_eq!(
        delegated_child_model_id(Some("current".to_owned()), &current_model),
        "provider/model"
    );
    assert_eq!(
        delegated_child_model_id(Some("  CURRENT  ".to_owned()), &current_model),
        "provider/model"
    );
    assert_eq!(
        delegated_child_model_id(Some("provider/other".to_owned()), &current_model),
        "provider/other"
    );
}

#[test]
fn delegated_children_and_direct_messages_are_durable_and_bounded() {
    let temp = std::env::temp_dir().join(format!("loom-project-agents-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&temp).unwrap();
    let backend = InProcessBackend::new_persistent(temp.join("state.sqlite")).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Project agents".into(),
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

    let create_spec = loom_core::DelegatedTaskSpec {
        intent: "Review a bounded task".into(),
        model_id: "deterministic/demo".into(),
        context_references: vec![],
        dependencies: vec![],
        code_change: false,
        permissions: loom_core::ProjectAgentPermissions::default(),
    };
    let request_id = RequestId::new();
    let response =
        connection.create_project_child(request_id, root, "worker-1".into(), create_spec.clone());
    let (task, child) = match response.unwrap() {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, child }) => {
            (task, child)
        }
        response => panic!("unexpected child response: {response:?}"),
    };
    assert_eq!(task.requester_session_id, root);
    assert_eq!(task.target_session_id, child.session_id);
    assert_eq!(child.depth, 2);
    let child_project_snapshot = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::GetProjectSnapshotForSession {
            session_id: child.session_id,
        },
    )));
    assert!(matches!(
        child_project_snapshot.result,
        Ok(ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot)))
            if snapshot.project_id == task.project_id
                && snapshot.root_session_id == root
                && snapshot.agents.len() == 2
    ));
    assert!(matches!(
        connection.create_project_child(
            request_id,
            root,
            "worker-1".into(),
            create_spec.clone(),
        ),
        Ok(ServerResponse::Project(ProjectResponse::ProjectChildCreated{ task: repeated, child: repeated_child }))
            if repeated.task_id == task.task_id && repeated_child.session_id == child.session_id
    ));
    let snapshot = connection
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::GetProjectSnapshot {
                project_id: ProjectId::from_uuid(*root.as_uuid()),
            },
        )))
        .result
        .unwrap();
    assert!(
        matches!(snapshot, ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot)) if snapshot.agents.len() == 2)
    );
    let ServerResponse::Project(ProjectResponse::ProjectSnapshot(project_snapshot)) = connection
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::GetProjectSnapshot {
                project_id: ProjectId::from_uuid(*root.as_uuid()),
            },
        )))
        .result
        .unwrap()
    else {
        panic!("expected project snapshot")
    };
    assert!(
        project_snapshot
            .tasks
            .iter()
            .any(|candidate| candidate.task_id == task.task_id)
    );

    let message = loom_core::AgentMessageDraft {
        project_id: ProjectId::from_uuid(*root.as_uuid()),
        task_id: Some(task.task_id),
        sender_session_id: root,
        target_session_id: child.session_id,
        kind: loom_core::AgentMessageKind::Direction,
        body: "Please report findings.".into(),
    };
    for untrusted_message in [
        message.clone(),
        loom_core::AgentMessageDraft {
            sender_session_id: child.session_id,
            ..message.clone()
        },
    ] {
        assert_eq!(
            connection
                .request(RequestEnvelope::new(ClientRequest::Project(
                    ProjectRequest::SendProjectAgentMessage {
                        message: untrusted_message,
                    }
                )))
                .result
                .unwrap_err()
                .code,
            ErrorCode::AuthorizationDenied
        );
    }
    let root_events = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: Some(root),
            workspace_id: None,
            after_sequence: Some(EventSequence::default()),
            stream_epoch: None,
        },
    )));
    assert!(matches!(
        root_events.result,
        Ok(ServerResponse::Events(EventsResponse::SessionEvents{ events, .. }))
            if events.iter().all(|event| !matches!(
                &event.event,
                ServerEvent::ProjectAgentMessageAccepted { message: accepted }
                    if accepted.project_id == message.project_id
            ))
    ));
    let messages = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::ListProjectAgentMessages {
            project_id: message.project_id,
            session_id: child.session_id,
            after_project_sequence: None,
            limit: 10,
        },
    )));
    assert!(matches!(
        messages.result,
        Ok(ServerResponse::Project(ProjectResponse::ProjectAgentMessages{ messages, .. })) if messages.is_empty()
    ));
    let tokens = AuthTokenStore::new();
    let scoped_token = tokens
        .issue(AuthorizationScope::for_sessions(
            [root],
            backend.supported_capabilities.clone(),
        ))
        .unwrap();
    let scoped = backend.connect_authenticated(tokens.authenticate(&scoped_token.token).unwrap());
    negotiate(&scoped);
    assert_eq!(
        scoped
            .request(RequestEnvelope::new(ClientRequest::Project(
                ProjectRequest::SendProjectAgentMessage {
                    message: message.clone(),
                }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::AuthorizationDenied
    );
    let broad_token = tokens.issue(AuthorizationScope::all()).unwrap();
    let broad = backend.connect_authenticated(tokens.authenticate(&broad_token.token).unwrap());
    negotiate(&broad);
    assert_eq!(
        broad
            .request(RequestEnvelope::new(ClientRequest::Project(
                ProjectRequest::SendProjectAgentMessage {
                    message: message.clone(),
                }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::AuthorizationDenied
    );
    assert_eq!(
        scoped
            .request(RequestEnvelope::new(ClientRequest::Project(
                ProjectRequest::ListProjectAgentMessages {
                    project_id: message.project_id,
                    session_id: child.session_id,
                    after_project_sequence: None,
                    limit: 10,
                }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::AuthorizationDenied
    );
    let wrong_direction = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::SendProjectAgentMessage {
            message: loom_core::AgentMessageDraft {
                target_session_id: AgentSessionId::new(),
                ..message.clone()
            },
        },
    )));
    assert!(matches!(
        wrong_direction.result,
        Err(error) if error.code == ErrorCode::AuthorizationDenied
    ));
    let code_change = connection.create_project_child(
        RequestId::new(),
        root,
        "coder".into(),
        loom_core::DelegatedTaskSpec {
            intent: "Change code".into(),
            model_id: "deterministic/demo".into(),
            context_references: vec![],
            dependencies: vec![],
            code_change: true,
            permissions: loom_core::ProjectAgentPermissions::default(),
        },
    );
    assert!(matches!(
        code_change,
        Err(ref error)
            if error.code == ErrorCode::InvalidRequest
                && error.message.contains("exactly one Git repository")
    ));

    assert!(matches!(
        connection.create_project_child(
            request_id,
            root,
            "different-child".into(),
            loom_core::DelegatedTaskSpec {
                intent: "Changed request under reused ID".into(),
                model_id: "deterministic/demo".into(),
                context_references: vec![],
                dependencies: vec![],
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions::default(),
            },
        ),
        Err(error) if error.code == ErrorCode::InvalidRequest
    ));
    // Delegated children start background runs on the shared process-wide
    // executor. Nothing below depends on the child runs, so drain the backend
    // (stopping and joining those workers) before deleting its directory;
    // otherwise a late worker recreates files under it during removal and
    // `remove_dir_all` fails with `DirectoryNotEmpty`.
    drop(connection);
    backend.shutdown().unwrap();
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn project_manager_tool_creates_an_idempotent_non_code_child() {
    let temp = std::env::temp_dir().join(format!(
        "loom-project-delegation-tool-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let backend = InProcessBackend::new_persistent(temp.join("state.sqlite")).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Project tool test".into(),
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
        connection
            .project_delegation_enabled_for_session(root)
            .unwrap()
    );
    let tokens = AuthTokenStore::new();
    let read_only = tokens
        .issue(AuthorizationScope::for_sessions(
            [root],
            CapabilitySet::new([Capability::StartAgentRun, Capability::ReadAgentSession]),
        ))
        .unwrap();
    let read_only_connection =
        backend.connect_authenticated(tokens.authenticate(&read_only.token).unwrap());
    assert!(
        !read_only_connection
            .project_delegation_enabled_for_session(root)
            .unwrap()
    );
    let delegation_grant = tokens
        .issue(AuthorizationScope::for_sessions(
            [root],
            CapabilitySet::new([Capability::StartAgentRun, Capability::CreateProjectChild]),
        ))
        .unwrap();
    let delegation_connection =
        backend.connect_authenticated(tokens.authenticate(&delegation_grant.token).unwrap());
    assert!(
        delegation_connection
            .project_delegation_enabled_for_session(root)
            .unwrap()
    );
    assert!(
        !delegation_connection
            .project_capability_enabled_for_session(root, Capability::SendProjectAgentMessage)
            .unwrap()
    );
    let coordination_grant = tokens
        .issue(AuthorizationScope::for_sessions(
            [root],
            CapabilitySet::new([
                Capability::StartAgentRun,
                Capability::SendProjectAgentMessage,
                Capability::ReadProject,
            ]),
        ))
        .unwrap();
    let coordination_connection =
        backend.connect_authenticated(tokens.authenticate(&coordination_grant.token).unwrap());
    assert!(
        coordination_connection
            .project_capability_enabled_for_session(root, Capability::SendProjectAgentMessage)
            .unwrap()
    );
    assert!(
        coordination_connection
            .project_capability_enabled_for_session(root, Capability::ReadProject)
            .unwrap()
    );
    let control_grant = tokens
        .issue(AuthorizationScope::for_sessions(
            [root],
            CapabilitySet::new([Capability::StartAgentRun, Capability::ControlProjectChild]),
        ))
        .unwrap();
    let control_connection =
        backend.connect_authenticated(tokens.authenticate(&control_grant.token).unwrap());
    assert!(
        control_connection
            .project_capability_enabled_for_session(root, Capability::ControlProjectChild)
            .unwrap()
    );
    assert!(
        !delegation_connection
            .project_capability_enabled_for_session(root, Capability::ControlProjectChild)
            .unwrap()
    );
    let extension = backend
        .project_agent_tools(
            root,
            ModelId::new("deterministic/demo"),
            ProjectAgentToolGrants {
                delegation: true,
                messaging: true,
                branch_messaging: true,
                inspection: true,
                child_control: true,
                worktree: false,
                review: false,
                integration: false,
            },
        )
        .unwrap()
        .expect("root project tools");
    let tools = ToolExecutor::new_with_workspace(connection.session_filesystem(root).unwrap())
        .with_extension(extension);
    let call = ToolCall {
        id: ToolCallId::new(),
        name: "delegate_project_task".to_owned(),
        arguments: serde_json::json!({
            "child_name": "protocol reviewer",
            "intent": "Review the protocol compatibility design",
            "context_references": [{"label": "Design", "uri": "docs/project-sessions-design.md"}],
            "dependencies": [],
            "permissions": {"branch_messaging": true}
        }),
    };

    assert!(
        tools
            .definitions()
            .iter()
            .any(|definition| definition.name == "delegate_project_task")
    );
    assert!(
        tools
            .definitions()
            .iter()
            .any(|definition| definition.name == "send_project_agent_message")
    );
    assert!(
        tools
            .definitions()
            .iter()
            .any(|definition| definition.name == "list_project_children")
    );
    assert!(
        tools
            .definitions()
            .iter()
            .any(|definition| definition.name == "control_project_child")
    );
    assert_eq!(tools.action_kind(&call), Some(loom_core::ActionKind::Write));
    let first = tools.execute(&call);
    assert!(first.success, "{}", first.output);
    let first_output: serde_json::Value = serde_json::from_str(&first.output).unwrap();
    let task_id = first_output["task_id"].as_str().unwrap();
    let child_session_id = first_output["child_session_id"]
        .as_str()
        .unwrap()
        .parse::<AgentSessionId>()
        .unwrap();
    let repeated = tools.execute(&call);
    assert!(repeated.success, "{}", repeated.output);
    let repeated_output: serde_json::Value = serde_json::from_str(&repeated.output).unwrap();
    assert_eq!(repeated_output["task_id"].as_str(), Some(task_id));

    let snapshot = backend
        .persistence
        .as_ref()
        .unwrap()
        .load_project_snapshot(ProjectId::from_uuid(*root.as_uuid()))
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.agents.len(), 2);
    assert!(
        snapshot
            .tasks
            .iter()
            .any(|task| task.task_id.to_string() == task_id)
    );
    let task_id = task_id.parse::<loom_core::TaskId>().unwrap();
    let task_id_string = task_id.to_string();
    let task = backend
        .persistence
        .as_ref()
        .unwrap()
        .load_delegated_task(task_id)
        .unwrap()
        .unwrap();
    assert_eq!(task.model_id, "deterministic/demo");
    assert!(!task.code_change);

    let ungranted_peer = tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "delegate_project_task".to_owned(),
        arguments: serde_json::json!({
            "child_name": "ungranted peer",
            "intent": "Remain available for a bounded follow-up",
        }),
    });
    assert!(ungranted_peer.success, "{}", ungranted_peer.output);
    let ungranted_peer: serde_json::Value = serde_json::from_str(&ungranted_peer.output).unwrap();
    let ungranted_peer_id = ungranted_peer["child_session_id"]
        .as_str()
        .unwrap()
        .parse::<AgentSessionId>()
        .unwrap();

    // Keep a second child idle so the manager-to-child tool path does not
    // race the deterministic child runner finishing its first task.
    let idle_child_id = AgentSessionId::new();
    let created_at = Timestamp::now();
    let idle_child = AgentSessionSnapshot {
        id: idle_child_id,
        workspace_id: workspace.id,
        name: "idle reviewer".to_owned(),
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
    };
    let idle_task = loom_core::DelegatedTaskRecord {
        task_id: loom_core::TaskId::new(),
        project_id: ProjectId::from_uuid(*root.as_uuid()),
        requester_session_id: root,
        target_session_id: idle_child_id,
        child_name: idle_child.name.clone(),
        intent: "Wait for manager direction".to_owned(),
        model_id: "deterministic/demo".to_owned(),
        context_references: vec![],
        dependencies: vec![],
        code_change: false,
        permissions: loom_core::ProjectAgentPermissions {
            branch_messaging: true,
            ..loom_core::ProjectAgentPermissions::default()
        },
        status: loom_core::DelegatedTaskStatus::Queued,
        created_at,
        updated_at: created_at,
    };
    let persistence = backend.persistence.as_ref().unwrap();
    persistence
        .create_project_child(
            RequestId::new(),
            &idle_child,
            backend.sessions().unwrap().next_sequence().next(),
            &idle_task,
        )
        .unwrap();
    let (_, created_event) = backend
        .sessions()
        .unwrap()
        .create_in_workspace_with_id(workspace.id, idle_child_id, idle_child.name.clone())
        .unwrap();
    backend.journal().unwrap().append_session(created_event);
    let filesystem = backend
        .create_session_filesystem(workspace.id, idle_child_id)
        .unwrap();
    backend
        .session_filesystems()
        .unwrap()
        .insert(idle_child_id, filesystem);
    backend
        .session_repositories()
        .unwrap()
        .insert(idle_child_id, BTreeMap::new());
    let direction_call = ToolCall {
        id: ToolCallId::new(),
        name: "send_project_agent_message".to_owned(),
        arguments: serde_json::json!({
            "target_session_id": idle_child_id,
            "task_id": idle_task.task_id,
            "kind": "direction",
            "body": "Review the current protocol gate and report back."
        }),
    };
    let direction = tools.execute(&direction_call);
    assert!(direction.success, "{}", direction.output);
    let child_inbox = persistence
        .list_agent_messages(idle_task.project_id, idle_child_id, 0, 10)
        .unwrap();
    assert_eq!(child_inbox.len(), 1);
    assert_eq!(child_inbox[0].sender_session_id, root);

    let branch_extension = backend
        .project_agent_tools(
            child_session_id,
            ModelId::new("deterministic/demo"),
            ProjectAgentToolGrants {
                delegation: false,
                messaging: true,
                branch_messaging: true,
                inspection: false,
                child_control: false,
                worktree: false,
                review: false,
                integration: false,
            },
        )
        .unwrap()
        .expect("branch-messaging project tools");
    let branch_tools =
        ToolExecutor::new_with_workspace(connection.session_filesystem(child_session_id).unwrap())
            .with_extension(branch_extension);
    let recipients = branch_tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "list_project_message_recipients".to_owned(),
        arguments: serde_json::json!({}),
    });
    assert!(recipients.success, "{}", recipients.output);
    let recipients: serde_json::Value = serde_json::from_str(&recipients.output).unwrap();
    assert_eq!(recipients.as_array().unwrap().len(), 1);
    assert_eq!(
        recipients[0]["session_id"].as_str(),
        Some(idle_child_id.to_string().as_str())
    );
    let branch_message = branch_tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "send_project_agent_message".to_owned(),
        arguments: serde_json::json!({
            "target_session_id": idle_child_id,
            "task_id": task.task_id,
            "kind": "progress",
            "body": "I found a related point that may help your review."
        }),
    });
    assert!(branch_message.success, "{}", branch_message.output);
    let denied_branch_message = branch_tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "send_project_agent_message".to_owned(),
        arguments: serde_json::json!({
            "target_session_id": ungranted_peer_id,
            "kind": "progress",
            "body": "This recipient has no branch-messaging grant."
        }),
    });
    assert!(!denied_branch_message.success);
    let idle_inbox = persistence
        .list_agent_messages(idle_task.project_id, idle_child_id, 0, 10)
        .unwrap();
    assert_eq!(idle_inbox.len(), 2);
    assert_eq!(idle_inbox[1].sender_session_id, child_session_id);
    assert_eq!(idle_inbox[1].target_session_id, idle_child_id);
    let root_branch_inbox = persistence
        .list_agent_messages(idle_task.project_id, root, 0, 10)
        .unwrap();
    assert!(root_branch_inbox.is_empty());

    let cancel_call = ToolCall {
        id: ToolCallId::new(),
        name: "control_project_child".to_owned(),
        arguments: serde_json::json!({
            "task_id": idle_task.task_id,
            "action": "cancel"
        }),
    };
    assert_eq!(
        tools.action_kind(&cancel_call),
        Some(loom_core::ActionKind::Write)
    );
    let cancellation = tools.execute(&cancel_call);
    assert!(cancellation.success, "{}", cancellation.output);
    let direct_control = connection
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::ControlProjectChild {
                project_id: idle_task.project_id,
                manager_session_id: root,
                task_id: idle_task.task_id,
                action: ProjectChildControlAction::Cancel,
            },
        )))
        .result
        .unwrap();
    assert!(matches!(
        direct_control,
        ServerResponse::Project(ProjectResponse::ProjectChildControlled{ task, .. })
            if task.status == loom_core::DelegatedTaskStatus::Cancelled
    ));
    assert_eq!(
        persistence
            .load_delegated_task(idle_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Cancelled
    );
    let after_cancel_message = ToolCall {
        id: ToolCallId::new(),
        name: "send_project_agent_message".to_owned(),
        arguments: serde_json::json!({
            "target_session_id": idle_child_id,
            "task_id": idle_task.task_id,
            "kind": "direction",
            "body": "This new message must be rejected after cancellation."
        }),
    };
    assert!(!tools.execute(&after_cancel_message).success);

    let inspect_call = ToolCall {
        id: ToolCallId::new(),
        name: "list_project_children".to_owned(),
        arguments: serde_json::json!({}),
    };
    let inspection = tools.execute(&inspect_call);
    assert!(inspection.success, "{}", inspection.output);
    let inspection: serde_json::Value = serde_json::from_str(&inspection.output).unwrap();
    let child_session_id_string = child_session_id.to_string();
    assert!(
        inspection["children"]
            .as_array()
            .unwrap()
            .iter()
            .any(|child| {
                child["session_id"].as_str() == Some(child_session_id_string.as_str())
                    && child["task_id"].as_str() == Some(task_id_string.as_str())
            })
    );

    let child_extension = backend
        .project_agent_tools(
            child_session_id,
            ModelId::new("deterministic/demo"),
            ProjectAgentToolGrants {
                delegation: false,
                messaging: true,
                branch_messaging: false,
                inspection: false,
                child_control: false,
                worktree: false,
                review: false,
                integration: false,
            },
        )
        .unwrap()
        .expect("child message tool");
    let child_tools =
        ToolExecutor::new_with_workspace(connection.session_filesystem(child_session_id).unwrap())
            .with_extension(child_extension);
    let report_call = ToolCall {
        id: ToolCallId::new(),
        name: "send_project_agent_message".to_owned(),
        arguments: serde_json::json!({
            "target_session_id": root,
            "kind": "result",
            "body": "Protocol review complete; the upgrade boundary is explicit."
        }),
    };
    assert_eq!(
        child_tools.action_kind(&report_call),
        Some(loom_core::ActionKind::Read)
    );
    let report = child_tools.execute(&report_call);
    assert!(report.success, "{}", report.output);
    let report_retry = child_tools.execute(&report_call);
    assert!(report_retry.success, "{}", report_retry.output);
    let root_inbox = backend
        .persistence
        .as_ref()
        .unwrap()
        .list_agent_messages(ProjectId::from_uuid(*root.as_uuid()), root, 0, 10)
        .unwrap();
    assert_eq!(root_inbox.len(), 1);
    assert_eq!(root_inbox[0].sender_session_id, child_session_id);
    assert_eq!(root_inbox[0].target_session_id, root);
    drop(connection);
    backend.shutdown().unwrap();
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn user_direction_reaches_a_parked_project_manager() {
    let temp = std::env::temp_dir().join(format!(
        "loom-project-manager-direction-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/project-manager-direction");
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
                name: "Project manager direction e2e".to_owned(),
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

    let root_run_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRun {
                session_id: root,
                task: "Coordinate a bounded investigation with a sub-agent.".to_owned(),
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

    // Turn one delegates a non-code child.
    let root_delegate = model.next_for_manager();
    assert!(request_has_tool(
        &root_delegate.request,
        "delegate_project_task"
    ));
    model.respond_with_tool(
        root_delegate,
        "delegate_project_task",
        serde_json::json!({
            "child_name": "investigator",
            "intent": "Complete a short investigation and report the finding.",
            "model_id": model_id,
        }),
    );

    // Turn two parks the manager on the child.
    let root_wait = model.next_for_manager();
    assert!(request_has_tool(
        &root_wait.request,
        "wait_for_project_children"
    ));
    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let persistence = backend.persistence.as_ref().unwrap();
    let child_task = persistence
        .list_project_tasks(project_id)
        .unwrap()
        .into_iter()
        .find(|task| task.child_name == "investigator")
        .expect("root delegation should create the investigator task");
    model.respond_with_tool(
        root_wait,
        "wait_for_project_children",
        serde_json::json!({ "task_ids": [child_task.task_id] }),
    );

    assert_eq!(
        await_settled_run(&connection, root_run_id).state,
        AgentRunState::Paused
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while persistence
        .load_delegated_task(child_task.task_id)
        .unwrap()
        .unwrap()
        .status
        != loom_core::DelegatedTaskStatus::Running
    {
        assert!(
            Instant::now() < deadline,
            "parking the manager should release its slot and admit the child"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let wait = persistence
        .list_project_manager_waits_by_child(child_task.task_id)
        .unwrap()
        .into_iter()
        .find(|wait| wait.manager_session_id == root)
        .expect("parked manager wait should be durable");
    assert_eq!(wait.status, loom_core::ProjectManagerWaitStatus::Waiting);

    let parked = await_settled_run(&connection, root_run_id);
    assert_eq!(parked.state, AgentRunState::Paused);

    // A user direction must reach the parked manager instead of being rejected.
    connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::SendAgentMessage {
                run_id: root_run_id,
                attempt_id: parked.attempt_id,
                expected_control_revision: parked.control_revision,
                message: "Change of plan: summarize what you have so far.".to_owned(),
            },
        )))
        .result
        .unwrap();

    let redirected = model.next_for_manager();
    let messages = redirected.request["messages"].as_array().unwrap();
    assert!(
        messages.iter().any(|message| {
            message["role"] == "user"
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("Change of plan"))
        }),
        "the manager should see the new user direction"
    );
    assert!(
        messages.iter().any(|message| {
            message["role"] == "tool"
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("superseded"))
        }),
        "the superseded wait should be reported back to the manager"
    );
    model.respond_with_tool(
        redirected,
        "ask_user",
        serde_json::json!({"prompt": "Which summary format should I use?"}),
    );

    assert_eq!(
        await_settled_run(&connection, root_run_id).state,
        AgentRunState::NeedsInput
    );
    await_project_manager_wait_status(
        persistence,
        wait.wait_id,
        loom_core::ProjectManagerWaitStatus::Abandoned,
    );

    // The child continues independently and can still finish.
    let child_run_id = persistence
        .load_latest_run_summary_for_session(child_task.target_session_id)
        .unwrap()
        .expect("admitted child should have a durable run")
        .snapshot
        .id;
    let child_turn = model.next_for_child();
    model.respond_with_text(child_turn, "Investigation complete.");
    assert_eq!(
        await_settled_run(&connection, child_run_id).state,
        AgentRunState::Completed
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn child_completion_wakes_a_finished_manager() {
    let temp = std::env::temp_dir().join(format!("loom-child-wake-e2e-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&temp).unwrap();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/child-wake");
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
                name: "Child wake e2e".to_owned(),
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
                task: "Delegate a bounded task and continue with me meanwhile.".to_owned(),
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

    // Turn one delegates a non-code child.
    let root_delegate = model.next_for_manager();
    assert!(request_has_tool(
        &root_delegate.request,
        "delegate_project_task"
    ));
    model.respond_with_tool(
        root_delegate,
        "delegate_project_task",
        serde_json::json!({
            "child_name": "worker",
            "intent": "Do a short piece of work.",
            "model_id": model_id,
        }),
    );

    // Turn two replies to the user and ends the turn. An active child no longer
    // blocks completion, which is what makes "continue here" work.
    let root_reply = model.next_for_manager();
    model.respond_with_text(root_reply, "Delegated. Continuing with you now.");
    assert_eq!(
        await_settled_run(&connection, root_run_id).state,
        AgentRunState::Completed
    );

    let child_task = persistence
        .list_project_tasks(project_id)
        .unwrap()
        .into_iter()
        .find(|task| task.child_name == "worker")
        .expect("delegation should create the worker task");
    assert_eq!(child_task.status, loom_core::DelegatedTaskStatus::Running);
    let child_run_id = persistence
        .load_latest_run_summary_for_session(child_task.target_session_id)
        .unwrap()
        .expect("child should have a durable run")
        .snapshot
        .id;

    // The child finishes without sending its own result; the server synthesizes
    // a durable result and wakes the finished manager.
    let child_turn = model.next_for_child();
    model.respond_with_text(child_turn, "Child work complete.");
    assert_eq!(
        await_settled_run(&connection, child_run_id).state,
        AgentRunState::Completed
    );

    let woken = model.next_for_manager();
    let messages = woken.request["messages"].as_array().unwrap();
    assert!(
        messages.iter().any(|message| {
            message["name"] == "loom_project_message"
                && message["content"].as_str().is_some_and(|content| {
                    content.contains("worker") && content.contains("Completed")
                })
        }),
        "the woken manager should see the child result"
    );
    model.respond_with_text(woken, "Thanks, I have the child result.");
    assert_eq!(
        await_settled_run(&connection, root_run_id).state,
        AgentRunState::Completed
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn code_child_completion_wakes_a_finished_manager() {
    let temp = workspace();
    let source = git_repository();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/code-child-wake");
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
                name: "Code child wake e2e".to_owned(),
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
    assert!(matches!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Repository(
                RepositoryRequest::AttachSessionRepository {
                    session_id: root,
                    source: source.display().to_string(),
                    path: "repo".to_owned(),
                    revision: None,
                    reuse_local: false,
                },
            )))
            .result,
        Ok(ServerResponse::Repository(
            RepositoryResponse::SessionRepositoryAttached(_)
        ))
    ));
    let root_run_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRun {
                session_id: root,
                task: "Delegate a code task and continue with me meanwhile.".to_owned(),
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

    // Turn one delegates a code-changing child.
    let root_delegate = model.next_for_manager();
    assert!(request_has_tool(
        &root_delegate.request,
        "delegate_project_code_task"
    ));
    model.respond_with_tool(
        root_delegate,
        "delegate_project_code_task",
        serde_json::json!({
            "child_name": "code-worker",
            "intent": "Make a small committed change.",
            "model_id": model_id,
        }),
    );

    // Turn two replies to the user and ends the turn. An active code child no
    // longer blocks completion, which is what makes "continue here" work.
    let root_reply = model.next_for_manager();
    model.respond_with_text(root_reply, "Delegated. Continuing with you now.");
    assert_eq!(
        await_settled_run(&connection, root_run_id).state,
        AgentRunState::Completed
    );

    let child_task = persistence
        .list_project_tasks(project_id)
        .unwrap()
        .into_iter()
        .find(|task| task.child_name == "code-worker")
        .expect("delegation should create the code worker task");
    assert!(child_task.code_change);
    assert_eq!(child_task.status, loom_core::DelegatedTaskStatus::Running);
    let child_run_id = persistence
        .load_latest_run_summary_for_session(child_task.target_session_id)
        .unwrap()
        .expect("code child should have a durable run")
        .snapshot
        .id;

    // The child finishes without sending its own result; the server synthesizes
    // a durable result and wakes the finished manager.
    let child_turn = model.next_for_child();
    model.respond_with_text(child_turn, "Code child work complete.");
    assert_eq!(
        await_settled_run(&connection, child_run_id).state,
        AgentRunState::Completed
    );

    let woken = model.next_for_manager();
    let messages = woken.request["messages"].as_array().unwrap();
    assert!(
        messages.iter().any(|message| {
            message["name"] == "loom_project_message"
                && message["content"].as_str().is_some_and(|content| {
                    content.contains("code-worker") && content.contains("Completed")
                })
        }),
        "the woken manager should see the code child result"
    );
    // The reviewed-result guard still applies to completed code children, so
    // the woken manager reviews before it can finish its turn.
    model.respond_with_tool(
        woken,
        "review_project_child",
        serde_json::json!({ "task_id": child_task.task_id }),
    );
    let woken_after_review = model.next_for_manager();
    model.respond_with_text(
        woken_after_review,
        "Reviewed the code child result; nothing further to integrate.",
    );
    let woken_run_id = persistence
        .load_latest_run_summary_for_session(root)
        .unwrap()
        .expect("woken manager should have a durable run")
        .snapshot
        .id;
    assert_eq!(
        await_settled_run(&connection, woken_run_id).state,
        AgentRunState::Completed
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn durable_manager_wait_releases_workspace_slot_and_resumes_once() {
    let temp = std::env::temp_dir().join(format!(
        "loom-project-manager-wait-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/project-manager-wait");
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
                name: "Project manager wait e2e".to_owned(),
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

    let root_run_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRun {
                session_id: root,
                task: "Coordinate a bounded investigation with a submanager.".to_owned(),
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

    let root_delegate = model.next_for_manager();
    assert!(request_has_tool(
        &root_delegate.request,
        "delegate_project_task"
    ));
    model.respond_with_tool(
        root_delegate,
        "delegate_project_task",
        serde_json::json!({
            "child_name": "submanager",
            "intent": "Delegate one focused investigation, wait for it, and report the result.",
            "model_id": model_id,
            "permissions": { "delegation": true, "inspection": true }
        }),
    );
    // Keep the root's next turn open while the delegated manager runs.
    let root_followup = model.next_for_manager();

    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let persistence = backend.persistence.as_ref().unwrap();
    let manager_task = persistence
        .list_project_tasks(project_id)
        .unwrap()
        .into_iter()
        .find(|task| task.child_name == "submanager")
        .expect("root delegation should create the submanager task");

    let manager_delegate = model.next_for_child();
    assert!(request_has_tool(
        &manager_delegate.request,
        "delegate_project_task"
    ));
    model.respond_with_tool(
        manager_delegate,
        "delegate_project_task",
        serde_json::json!({
            "child_name": "investigator",
            "intent": "Complete a short investigation and report the finding.",
            "model_id": model_id,
            "dependencies": []
        }),
    );

    let manager_wait = model.next_for_child();
    assert!(request_has_tool(
        &manager_wait.request,
        "wait_for_project_children"
    ));
    let grandchild_task = persistence
        .list_project_tasks(project_id)
        .unwrap()
        .into_iter()
        .find(|task| task.requester_session_id == manager_task.target_session_id)
        .expect("submanager delegation should create its child task");
    assert_eq!(
        grandchild_task.status,
        loom_core::DelegatedTaskStatus::Queued
    );
    let manager_run_id = persistence
        .load_latest_run_summary_for_session(manager_task.target_session_id)
        .unwrap()
        .expect("submanager run should have a durable checkpoint")
        .snapshot
        .id;
    model.respond_with_tool(
        manager_wait,
        "wait_for_project_children",
        serde_json::json!({ "task_ids": [grandchild_task.task_id] }),
    );

    assert_eq!(
        await_settled_run(&connection, manager_run_id).state,
        AgentRunState::Paused
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let (manager_task_after_park, grandchild_task_after_park) = loop {
        let manager_task = persistence
            .load_delegated_task(manager_task.task_id)
            .unwrap()
            .unwrap();
        let grandchild_task = persistence
            .load_delegated_task(grandchild_task.task_id)
            .unwrap()
            .unwrap();
        if manager_task.status == loom_core::DelegatedTaskStatus::Blocked
            && grandchild_task.status == loom_core::DelegatedTaskStatus::Running
        {
            break (manager_task, grandchild_task);
        }
        assert!(
            Instant::now() < deadline,
            "parked manager should release its slot and start its queued child; manager={:?}, child={:?}",
            manager_task.status,
            grandchild_task.status
        );
        thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(
        manager_task_after_park.status,
        loom_core::DelegatedTaskStatus::Blocked
    );
    assert_eq!(
        grandchild_task_after_park.status,
        loom_core::DelegatedTaskStatus::Running
    );
    let wait = persistence
        .list_project_manager_waits_by_child(grandchild_task.task_id)
        .unwrap()
        .into_iter()
        .find(|wait| wait.manager_session_id == manager_task.target_session_id)
        .expect("parked submanager wait should be durable");
    assert_eq!(wait.status, loom_core::ProjectManagerWaitStatus::Waiting);

    let deadline = Instant::now() + Duration::from_secs(2);
    while Timestamp::now() <= wait.created_at {
        assert!(
            Instant::now() < deadline,
            "clock should advance past the durable wait timestamp before creating a newer task"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let sibling = connection
        .create_project_child(
            RequestId::new(),
            root,
            "root-sibling".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Complete a short independent follow-up.".to_owned(),
                model_id: model_id.as_str().to_owned(),
                context_references: vec![],
                dependencies: vec![],
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions::default(),
            },
        )
        .unwrap();
    let sibling_task = match sibling {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, .. }) => task,
        response => panic!("unexpected sibling task response: {response:?}"),
    };
    assert!(sibling_task.created_at > wait.created_at);
    assert_eq!(sibling_task.status, loom_core::DelegatedTaskStatus::Queued);

    let grandchild_turn = model.next_for_child();
    model.respond_with_text(
        grandchild_turn,
        "The investigation is complete: the finding is confirmed.",
    );
    assert_eq!(
        await_settled_run(
            &connection,
            persistence
                .load_latest_run_summary_for_session(grandchild_task.target_session_id)
                .unwrap()
                .expect("grandchild run should have a durable checkpoint")
                .snapshot
                .id
        )
        .state,
        AgentRunState::Completed
    );

    let resumed_manager_turn = model.next_for_child();
    let wait_call_id = wait.tool_call_id.to_string();
    let wait_results = resumed_manager_turn.request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| {
            message["role"] == "tool"
                && message["tool_call_id"] == wait_call_id
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("\"return_ready\":true"))
        })
        .count();
    assert_eq!(wait_results, 1, "the durable join should resume once");
    assert!(request_has_tool(
        &resumed_manager_turn.request,
        "wait_for_project_children"
    ));
    assert_eq!(
        persistence
            .load_delegated_task(manager_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Running
    );
    assert_eq!(
        persistence
            .load_delegated_task(sibling_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Queued,
        "the older ready join must claim the only workspace slot first"
    );
    assert_eq!(
        persistence
            .load_project_manager_wait(wait.wait_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::ProjectManagerWaitStatus::Resuming
    );
    model.respond_with_text(
        resumed_manager_turn,
        "The investigator confirmed the finding; the delegated work is complete.",
    );
    assert_eq!(
        await_settled_run(&connection, manager_run_id).state,
        AgentRunState::Completed
    );
    assert_eq!(
        await_project_manager_wait_status(
            persistence,
            wait.wait_id,
            loom_core::ProjectManagerWaitStatus::Consumed,
        )
        .status,
        loom_core::ProjectManagerWaitStatus::Consumed
    );
    let manager_join_results = persistence
        .load_run_messages(manager_run_id)
        .unwrap()
        .into_iter()
        .filter(|message| {
            message.role == loom_model::MessageRole::Tool
                && message.name.as_deref() == Some("wait_for_project_children")
                && message.tool_call_id == Some(wait.tool_call_id)
        })
        .count();
    assert_eq!(manager_join_results, 1);
    assert_eq!(
        persistence
            .load_delegated_task(grandchild_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Completed
    );
    let sibling_turn = model.next_for_child();
    assert_eq!(
        persistence
            .load_delegated_task(manager_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Completed
    );
    let sibling_run_id = persistence
        .load_latest_run_summary_for_session(sibling_task.target_session_id)
        .unwrap()
        .expect("root sibling should have started after the manager completed")
        .snapshot
        .id;
    assert_eq!(
        persistence
            .load_delegated_task(sibling_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Running
    );
    model.respond_with_text(sibling_turn, "The independent follow-up is complete.");
    assert_eq!(
        await_settled_run(&connection, sibling_run_id).state,
        AgentRunState::Completed
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = persistence
            .load_delegated_task(sibling_task.task_id)
            .unwrap()
            .unwrap()
            .status;
        if status == loom_core::DelegatedTaskStatus::Completed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "completed sibling run should release its delegated task slot; status={status:?}"
        );
        thread::sleep(Duration::from_millis(1));
    }

    model.respond_with_text(
        root_followup,
        "The submanager completed the investigation and confirmed the finding.",
    );
    assert_eq!(
        await_settled_run(&connection, root_run_id).state,
        AgentRunState::Completed
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn durable_manager_wait_recovers_after_restart_and_resumes_once() {
    let temp = std::env::temp_dir().join(format!(
        "loom-project-manager-wait-restart-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/project-manager-wait-restart");
    let provider_registry =
        || scripted_project_provider_registry(&model_endpoint, model_id.clone());
    let backend =
        InProcessBackend::with_provider_registry_persistent(provider_registry(), &persistence_path)
            .unwrap();
    // This exercises the persisted nested-delegation grant at depth two.

    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Project manager wait restart e2e".to_owned(),
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

    let (manager_task, manager_session) = match connection
        .create_project_child(
            RequestId::new(),
            root,
            "submanager".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Delegate one child, wait for it, and summarize its result.".to_owned(),
                model_id: model_id.as_str().to_owned(),
                context_references: vec![],
                dependencies: vec![],
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions {
                    delegation: true,
                    inspection: true,
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

    // Create an older queued prerequisite while the manager holds the
    // single workspace slot. After the manager parks, it takes the slot.
    // The grandchild depends on it, so the wait and queued grandchild stay
    // pending while the prerequisite is paused during shutdown.
    let sibling = connection
        .create_project_child(
            RequestId::new(),
            root,
            "older-sibling".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Complete a short independent follow-up.".to_owned(),
                model_id: model_id.as_str().to_owned(),
                context_references: vec![],
                dependencies: vec![],
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions::default(),
            },
        )
        .unwrap();
    let sibling_task = match sibling {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, .. }) => task,
        response => panic!("unexpected sibling task response: {response:?}"),
    };
    assert_eq!(sibling_task.status, loom_core::DelegatedTaskStatus::Queued);
    let timestamp_deadline = Instant::now() + Duration::from_secs(2);
    while Timestamp::now() <= sibling_task.created_at {
        assert!(
            Instant::now() < timestamp_deadline,
            "clock should advance before creating the joined grandchild"
        );
        thread::sleep(Duration::from_millis(1));
    }

    model.respond_with_tool(
        manager_delegate,
        "delegate_project_task",
        serde_json::json!({
            "child_name": "investigator",
            "intent": "Complete a bounded investigation and report the finding.",
            "model_id": model_id,
            "dependencies": [sibling_task.task_id]
        }),
    );
    let manager_wait = model.next_for_child();
    assert!(request_has_tool(
        &manager_wait.request,
        "wait_for_project_children"
    ));

    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let persistence = backend.persistence.as_ref().unwrap();
    let grandchild_task = persistence
        .list_project_tasks(project_id)
        .unwrap()
        .into_iter()
        .find(|task| task.requester_session_id == manager_session.session_id)
        .expect("manager delegation should create the grandchild task");
    assert!(sibling_task.created_at < grandchild_task.created_at);
    assert_eq!(
        grandchild_task.status,
        loom_core::DelegatedTaskStatus::Queued
    );
    let manager_run_id = persistence
        .load_latest_run_summary_for_session(manager_session.session_id)
        .unwrap()
        .expect("manager run should have a durable checkpoint")
        .snapshot
        .id;
    model.respond_with_tool(
        manager_wait,
        "wait_for_project_children",
        serde_json::json!({ "task_ids": [grandchild_task.task_id] }),
    );
    assert_eq!(
        await_settled_run(&connection, manager_run_id).state,
        AgentRunState::Paused
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let (manager_task_after_park, sibling_after_park, grandchild_after_park) = loop {
        let manager_task = persistence
            .load_delegated_task(manager_task.task_id)
            .unwrap()
            .unwrap();
        let sibling_task = persistence
            .load_delegated_task(sibling_task.task_id)
            .unwrap()
            .unwrap();
        let grandchild_task = persistence
            .load_delegated_task(grandchild_task.task_id)
            .unwrap()
            .unwrap();
        if manager_task.status == loom_core::DelegatedTaskStatus::Blocked
            && sibling_task.status == loom_core::DelegatedTaskStatus::Running
            && grandchild_task.status == loom_core::DelegatedTaskStatus::Queued
        {
            break (manager_task, sibling_task, grandchild_task);
        }
        assert!(
            Instant::now() < deadline,
            "parked manager should admit only the older sibling; manager={:?}, sibling={:?}, grandchild={:?}",
            manager_task.status,
            sibling_task.status,
            grandchild_task.status
        );
        thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(
        manager_task_after_park.status,
        loom_core::DelegatedTaskStatus::Blocked
    );
    assert_eq!(
        sibling_after_park.status,
        loom_core::DelegatedTaskStatus::Running
    );
    assert_eq!(
        grandchild_after_park.status,
        loom_core::DelegatedTaskStatus::Queued
    );
    let wait = persistence
        .list_project_manager_waits_by_child(grandchild_task.task_id)
        .unwrap()
        .into_iter()
        .find(|wait| wait.manager_session_id == manager_session.session_id)
        .expect("parked manager wait should be durable");
    assert_eq!(wait.status, loom_core::ProjectManagerWaitStatus::Waiting);
    let manager_execution = persistence
        .load_run_execution_state(manager_run_id)
        .unwrap()
        .expect("manager continuation should be persisted");
    assert!(manager_execution.pending_project_join.is_some());
    let sibling_turn = model.next_for_child();
    assert!(!request_has_tool(
        &sibling_turn.request,
        "wait_for_project_children"
    ));
    let sibling_stream = hold_scripted_model_stream_until_cancelled(sibling_turn);
    drop(connection);
    backend.shutdown().unwrap();
    sibling_stream.join().unwrap();
    assert_eq!(
        persistence
            .load_project_manager_wait(wait.wait_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::ProjectManagerWaitStatus::Waiting
    );
    assert!(
        persistence
            .load_run_execution_state(manager_run_id)
            .unwrap()
            .unwrap()
            .pending_project_join
            .is_some()
    );
    assert!(
        persistence
            .load_latest_run_summary_for_session(grandchild_task.target_session_id)
            .unwrap()
            .is_none()
    );
    drop(backend);

    let reopened =
        InProcessBackend::with_provider_registry_persistent(provider_registry(), &persistence_path)
            .unwrap();
    assert!(
        reopened
            .supported_capabilities
            .contains(Capability::CreateNestedProjectChild)
    );
    let reopened_connection = reopened.connect();
    negotiate(&reopened_connection);
    let reopened_persistence = reopened.persistence.as_ref().unwrap();
    let sibling_run_id = reopened_persistence
        .load_latest_run_summary_for_session(sibling_task.target_session_id)
        .unwrap()
        .expect("prerequisite run should be durable after shutdown")
        .snapshot
        .id;
    assert_eq!(
        reopened_persistence
            .load_run_summary(manager_run_id)
            .unwrap()
            .unwrap()
            .snapshot
            .state,
        AgentRunState::Paused
    );
    assert!(
        reopened_persistence
            .load_run_execution_state(manager_run_id)
            .unwrap()
            .unwrap()
            .pending_project_join
            .is_some()
    );
    assert_eq!(
        reopened_persistence
            .load_project_manager_wait(wait.wait_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::ProjectManagerWaitStatus::Waiting
    );
    assert_eq!(
        reopened_persistence
            .load_run_summary(sibling_run_id)
            .unwrap()
            .unwrap()
            .snapshot
            .state,
        AgentRunState::Paused
    );
    assert_eq!(
        reopened_persistence
            .load_delegated_task(sibling_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Blocked
    );
    assert_eq!(
        reopened_persistence
            .load_delegated_task(grandchild_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Queued
    );

    // The grandchild remains queued because its prerequisite was paused.
    // Resume and finish that prerequisite, then complete the admitted
    // grandchild; its completion should resume the manager exactly once.
    let resumed_prerequisite = reopened_connection.request(RequestEnvelope::new(
        ClientRequest::Run(RunRequest::ResumeAgentRun {
            run_id: sibling_run_id,
        }),
    ));
    let ServerResponse::Run(RunResponse::AgentRun(resumed_prerequisite)) =
        resumed_prerequisite.result.unwrap()
    else {
        panic!("unexpected prerequisite resume response");
    };
    assert_ne!(resumed_prerequisite.state, AgentRunState::Paused);
    let prerequisite_turn = model.next_for_child();
    model.respond_with_text(
        prerequisite_turn,
        "The independent prerequisite is complete.",
    );
    assert_eq!(
        await_settled_run(&reopened_connection, sibling_run_id).state,
        AgentRunState::Completed
    );

    let grandchild_turn = model.next_for_child();
    let grandchild_run_id = reopened_persistence
        .load_latest_run_summary_for_session(grandchild_task.target_session_id)
        .unwrap()
        .expect("restart should admit the queued grandchild")
        .snapshot
        .id;
    model.respond_with_text(
        grandchild_turn,
        "The investigation is complete: the finding is confirmed.",
    );
    assert_eq!(
        await_settled_run(&reopened_connection, grandchild_run_id).state,
        AgentRunState::Completed
    );

    let resumed_manager_turn = model.next_for_child();
    let wait_call_id = wait.tool_call_id.to_string();
    let wait_results = resumed_manager_turn.request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| {
            message["role"] == "tool"
                && message["tool_call_id"] == wait_call_id
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("\"return_ready\":true"))
        })
        .count();
    assert_eq!(
        wait_results, 1,
        "the persisted wait result should replay once"
    );
    assert_eq!(
        reopened_persistence
            .load_project_manager_wait(wait.wait_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::ProjectManagerWaitStatus::Resuming
    );
    model.respond_with_text(
        resumed_manager_turn,
        "The investigator confirmed the finding; delegated work is complete.",
    );
    assert_eq!(
        await_settled_run(&reopened_connection, manager_run_id).state,
        AgentRunState::Completed
    );
    assert_eq!(
        await_project_manager_wait_status(
            reopened_persistence,
            wait.wait_id,
            loom_core::ProjectManagerWaitStatus::Consumed,
        )
        .status,
        loom_core::ProjectManagerWaitStatus::Consumed
    );
    let durable_wait_results = reopened_persistence
        .load_run_messages(manager_run_id)
        .unwrap()
        .into_iter()
        .filter(|message| {
            message.role == loom_model::MessageRole::Tool
                && message.name.as_deref() == Some("wait_for_project_children")
                && message.tool_call_id == Some(wait.tool_call_id)
        })
        .count();
    assert_eq!(durable_wait_results, 1);
    assert_eq!(
        reopened_persistence
            .load_delegated_task(grandchild_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Completed
    );

    drop(reopened_connection);
    reopened.shutdown().unwrap();
    drop(reopened);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
}

#[test]
fn cancelled_prerequisite_blocks_child_and_releases_manager_wait_once() {
    dependency_failure_blocks_child_and_releases_manager_wait_once(false);
}

#[test]
fn failed_prerequisite_blocks_child_and_releases_manager_wait_once() {
    dependency_failure_blocks_child_and_releases_manager_wait_once(true);
}

#[test]
fn project_coordination_exchange_and_transcripts_survive_restart() {
    let temp = std::env::temp_dir().join(format!(
        "loom-project-coordination-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/project-coordination");
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
                name: "Project coordination e2e".to_owned(),
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
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id: root,
            task: "Coordinate a short investigation with a child agent.".to_owned(),
            model: model_id.clone(),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    let root_run_id = match started.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected run response: {response:?}"),
    };

    // The model is driven by this test. Hold the manager's first follow-up
    // request while the child reports, so the next manager turn is forced
    // to cross an inbox boundary after both messages are durable.
    let manager_delegate = model.next_for_manager();
    assert!(request_has_tool(
        &manager_delegate.request,
        "delegate_project_task"
    ));
    model.respond_with_tool(
        manager_delegate,
        "delegate_project_task",
        serde_json::json!({
            "child_name": "investigator",
            "intent": "Investigate the question and report an initial finding and any uncertainty.",
            "model_id": model_id,
            "context_references": [],
            "dependencies": []
        }),
    );

    let child_progress_turn = model.next_for_child();
    assert!(request_has_tool(
        &child_progress_turn.request,
        "send_project_agent_message"
    ));
    let task = backend
        .persistence
        .as_ref()
        .unwrap()
        .list_project_tasks(ProjectId::from_uuid(*root.as_uuid()))
        .unwrap()
        .into_iter()
        .next()
        .expect("delegated task was committed before child scheduling");
    model.respond_with_tool(
        child_progress_turn,
        "send_project_agent_message",
        serde_json::json!({
            "target_session_id": root,
            "task_id": task.task_id,
            "kind": "progress",
            "body": "I have started the investigation and am checking the key assumption."
        }),
    );

    let child_question_turn = model.next_for_child();
    model.respond_with_tool(
        child_question_turn,
        "send_project_agent_message",
        serde_json::json!({
            "target_session_id": root,
            "task_id": task.task_id,
            "kind": "question",
            "body": "Should I prioritize the launch timeline or the reliability tradeoff?"
        }),
    );

    // A request after the question tool call proves the child message has
    // been accepted. Keep that child turn open until the manager's answer
    // and direction are recorded; it will need another turn to consume them.
    let child_waiting_for_manager = model.next_for_child();
    assert!(!request_has_project_message(
        &child_waiting_for_manager.request,
        "progress"
    ));
    assert!(!request_has_project_message(
        &child_waiting_for_manager.request,
        "question"
    ));

    let manager_poll = model.next_for_manager();
    assert!(!request_has_project_message(
        &manager_poll.request,
        "question"
    ));
    model.respond_with_tool(manager_poll, "list_project_children", serde_json::json!({}));

    let manager_with_child_messages = model.next_for_manager();
    assert!(request_has_project_message(
        &manager_with_child_messages.request,
        "progress"
    ));
    assert!(request_has_project_message(
        &manager_with_child_messages.request,
        "question"
    ));
    model.respond_with_tool(
        manager_with_child_messages,
        "send_project_agent_message",
        serde_json::json!({
            "target_session_id": task.target_session_id,
            "task_id": task.task_id,
            "kind": "answer",
            "body": "Prioritize reliability first; include the launch timeline as a secondary consideration."
        }),
    );

    let manager_direction_turn = model.next_for_manager();
    model.respond_with_tool(
        manager_direction_turn,
        "send_project_agent_message",
        serde_json::json!({
            "target_session_id": task.target_session_id,
            "task_id": task.task_id,
            "kind": "direction",
            "body": "Redirect the investigation toward the reliability tradeoff and state one practical next step."
        }),
    );

    // Keep the manager's next call open until the child has consumed both
    // messages and sent its result. The list call below advances the
    // manager to a boundary where the durable result can be delivered.
    let manager_waiting_for_result = model.next_for_manager();
    assert!(!request_has_project_message(
        &manager_waiting_for_result.request,
        "result"
    ));

    model.respond_with_tool(
        child_waiting_for_manager,
        "list_files",
        serde_json::json!({ "path": "." }),
    );
    let child_with_manager_messages = model.next_for_child();
    assert!(request_has_project_message(
        &child_with_manager_messages.request,
        "answer"
    ));
    assert!(request_has_project_message(
        &child_with_manager_messages.request,
        "direction"
    ));
    model.respond_with_tool(
        child_with_manager_messages,
        "send_project_agent_message",
        serde_json::json!({
            "target_session_id": root,
            "task_id": task.task_id,
            "kind": "result",
            "body": "Reliability is the priority; the next step is to validate the failure-recovery path before setting the launch date."
        }),
    );
    let child_final_turn = model.next_for_child();
    model.respond_with_text(
        child_final_turn,
        "Investigation complete. Reliability should be validated before setting the launch date.",
    );

    model.respond_with_tool(
        manager_waiting_for_result,
        "list_project_children",
        serde_json::json!({}),
    );
    let manager_with_result = model.next_for_manager();
    assert!(request_has_project_message(
        &manager_with_result.request,
        "result"
    ));
    model.respond_with_text(
        manager_with_result,
        "The child completed the investigation: validate reliability recovery first, then set the launch date.",
    );

    assert_eq!(
        await_settled_run(&connection, root_run_id).state,
        AgentRunState::Completed
    );
    let child = backend
        .persistence
        .as_ref()
        .unwrap()
        .load_project_snapshot(ProjectId::from_uuid(*root.as_uuid()))
        .unwrap()
        .unwrap()
        .agents
        .into_iter()
        .find(|agent| agent.session_id != root)
        .expect("child agent was created");
    let child_run_id = backend
        .persistence
        .as_ref()
        .unwrap()
        .load_latest_run_summary_for_session(child.session_id)
        .unwrap()
        .unwrap()
        .snapshot
        .id;
    assert_eq!(
        await_settled_run(&connection, child_run_id).state,
        AgentRunState::Completed
    );

    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let durable = backend.persistence.as_ref().unwrap();
    let task = durable.load_delegated_task(task.task_id).unwrap().unwrap();
    assert_eq!(task.status, loom_core::DelegatedTaskStatus::Completed);
    let parent_inbox = durable
        .list_agent_messages(project_id, root, 0, 10)
        .unwrap();
    let child_inbox = durable
        .list_agent_messages(project_id, child.session_id, 0, 10)
        .unwrap();
    assert_eq!(
        parent_inbox
            .iter()
            .map(|message| message.kind)
            .collect::<Vec<_>>(),
        vec![
            loom_core::AgentMessageKind::Progress,
            loom_core::AgentMessageKind::Question,
            loom_core::AgentMessageKind::Result,
        ]
    );
    assert_eq!(
        child_inbox
            .iter()
            .map(|message| message.kind)
            .collect::<Vec<_>>(),
        vec![
            loom_core::AgentMessageKind::Answer,
            loom_core::AgentMessageKind::Direction,
        ]
    );
    assert!(parent_inbox.iter().all(|message| {
        message.project_id == project_id
            && message.task_id == Some(task.task_id)
            && message.sender_session_id == child.session_id
            && message.target_session_id == root
    }));
    assert!(child_inbox.iter().all(|message| {
        message.project_id == project_id
            && message.task_id == Some(task.task_id)
            && message.sender_session_id == root
            && message.target_session_id == child.session_id
    }));
    let child_cursor = durable
        .load_run_execution_state(child_run_id)
        .unwrap()
        .unwrap()
        .last_project_message_sequence;
    let manager_cursor = durable
        .load_run_execution_state(root_run_id)
        .unwrap()
        .unwrap()
        .last_project_message_sequence;
    assert_eq!(child_cursor, child_inbox.last().unwrap().project_sequence);
    assert_eq!(
        manager_cursor,
        parent_inbox.last().unwrap().project_sequence
    );
    let child_transcript = durable.load_run_messages(child_run_id).unwrap();
    assert!(child_transcript.iter().any(|message| {
        message.name.as_deref() == Some("loom_project_message")
            && message.content.contains("Prioritize reliability first")
    }));
    assert!(child_transcript.iter().any(|message| {
        message.name.as_deref() == Some("loom_project_message")
            && message.content.contains("Redirect the investigation")
    }));
    let manager_transcript = durable.load_run_messages(root_run_id).unwrap();
    assert!(manager_transcript.iter().any(|message| {
        message.name.as_deref() == Some("loom_project_message")
            && message.content.contains("checking the key assumption")
    }));
    assert!(manager_transcript.iter().any(|message| {
        message.name.as_deref() == Some("loom_project_message")
            && message
                .content
                .contains("Should I prioritize the launch timeline")
    }));
    assert!(manager_transcript.iter().any(|message| {
        message.name.as_deref() == Some("loom_project_message")
            && message.content.contains("failure-recovery path")
    }));
    let root_runtime_config = durable
        .load_run_runtime_config(root_run_id)
        .unwrap()
        .unwrap();
    assert!(root_runtime_config.project_delegation_enabled);
    assert!(root_runtime_config.project_messaging_enabled);
    assert!(root_runtime_config.project_inspection_enabled);
    assert!(root_runtime_config.project_child_control_enabled);
    let child_runtime_config = durable
        .load_run_runtime_config(child_run_id)
        .unwrap()
        .unwrap();
    assert!(child_runtime_config.project_messaging_enabled);
    assert!(!child_runtime_config.project_child_control_enabled);

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);

    let reopened =
        InProcessBackend::with_provider_registry_persistent(provider_registry(), &persistence_path)
            .unwrap();
    let reopened_connection = reopened.connect();
    negotiate(&reopened_connection);
    let project = reopened_connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::GetProjectSnapshot { project_id },
    )));
    let ServerResponse::Project(ProjectResponse::ProjectSnapshot(project)) =
        project.result.unwrap()
    else {
        panic!("unexpected recovered project response");
    };
    assert_eq!(project.agents.len(), 2);
    assert!(
        project
            .agents
            .iter()
            .all(|agent| agent.state == AgentSessionState::Completed)
    );
    assert!(project.agents.iter().any(|agent| {
        agent.session_id == child.session_id
            && agent.parent_session_id == Some(root)
            && agent.depth == 2
    }));
    assert_eq!(
        reopened
            .persistence
            .as_ref()
            .unwrap()
            .load_delegated_task(task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Completed
    );
    let recovered_parent_messages = reopened
        .persistence
        .as_ref()
        .unwrap()
        .list_agent_messages(project_id, root, 0, 10)
        .unwrap();
    let recovered_child_messages = reopened
        .persistence
        .as_ref()
        .unwrap()
        .list_agent_messages(project_id, child.session_id, 0, 10)
        .unwrap();
    assert_eq!(recovered_parent_messages, parent_inbox);
    assert_eq!(recovered_child_messages, child_inbox);
    assert_eq!(
        reopened
            .persistence
            .as_ref()
            .unwrap()
            .load_run_execution_state(child_run_id)
            .unwrap()
            .unwrap()
            .last_project_message_sequence,
        child_cursor
    );
    assert_eq!(
        reopened
            .persistence
            .as_ref()
            .unwrap()
            .load_run_execution_state(root_run_id)
            .unwrap()
            .unwrap()
            .last_project_message_sequence,
        manager_cursor
    );
    assert_eq!(
        reopened
            .persistence
            .as_ref()
            .unwrap()
            .load_run_messages(child_run_id)
            .unwrap(),
        child_transcript
    );
    assert_eq!(
        reopened
            .persistence
            .as_ref()
            .unwrap()
            .load_run_messages(root_run_id)
            .unwrap(),
        manager_transcript
    );
    let recovered_root_runtime_config = reopened
        .persistence
        .as_ref()
        .unwrap()
        .load_run_runtime_config(root_run_id)
        .unwrap()
        .unwrap();
    assert!(recovered_root_runtime_config.project_delegation_enabled);
    assert!(recovered_root_runtime_config.project_messaging_enabled);
    assert!(recovered_root_runtime_config.project_inspection_enabled);
    assert!(recovered_root_runtime_config.project_child_control_enabled);
    let recovered_child_runtime_config = reopened
        .persistence
        .as_ref()
        .unwrap()
        .load_run_runtime_config(child_run_id)
        .unwrap()
        .unwrap();
    assert!(recovered_child_runtime_config.project_messaging_enabled);
    assert!(!recovered_child_runtime_config.project_child_control_enabled);
    let child_detail = reopened_connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunSnapshot {
            run_id: child_run_id,
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRunSnapshot(child_detail)) =
        child_detail.result.unwrap()
    else {
        panic!("unexpected recovered child run response");
    };
    assert_eq!(child_detail.run.state, AgentRunState::Completed);
    assert!(child_detail.messages.iter().any(|message| {
        message.name.as_deref() == Some("loom_project_message")
            && message.content.contains("Prioritize reliability first")
    }));
    assert!(child_detail.messages.iter().any(|message| {
        message.name.as_deref() == Some("loom_project_message")
            && message.content.contains("Redirect the investigation")
    }));
    let root_detail = reopened_connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunSnapshot {
            run_id: root_run_id,
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRunSnapshot(root_detail)) =
        root_detail.result.unwrap()
    else {
        panic!("unexpected recovered manager run response");
    };
    assert_eq!(root_detail.run.state, AgentRunState::Completed);
    assert!(root_detail.messages.iter().any(|message| {
        message.name.as_deref() == Some("loom_project_message")
            && message.content.contains("checking the key assumption")
    }));

    drop(reopened_connection);
    reopened.shutdown().unwrap();
    drop(reopened);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
}
