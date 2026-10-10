use super::{
    ACTIVE_BACKEND_NODE_ENTRY_ID, SessionSourceChoice, SessionSourceDialogPurpose, SessionTreeNode,
    WorkerConnectionStage, WorkerConnectionState, WorkerNodeEntry, adjusted_cpu_pulse_threshold,
    adjusted_project_agent_concurrency, assigned_node_id, connection_placeholder,
    filter_session_tree, format_percentage, format_worker_node_resources, initial_worker_nodes,
    local_source_available, mark_worker_connection_failed, merge_node_sessions,
    next_severe_load_streak, order_session_nodes, project_child_control_actions,
    project_session_list_projection, project_session_list_projection_for_projects,
    project_snapshot_has_unloaded_agent_sessions, remove_worker_node_entry, safe_worker_url_label,
    session_id_for_request, session_list_projection, session_status_pill,
    session_tree_descendant_count, source_choice_is_allowed, source_dialog_initial_state,
    transition_worker_connection_to_connecting, uncovered_session_ids, update_worker_node_status,
    validate_model_for_node, worker_connection_failure_detail, worker_node_display_name,
    worker_url_embeds_credential,
};
use loom_core::{
    AgentSessionId, AgentSessionSnapshot, AgentSessionState, CapabilitySet, EventSequence, RunId,
    Timestamp, WorkspaceId,
};
use loom_core::{ErrorCode, LoomError};
use loom_model::ModelId;
use loom_protocol::{
    ClientRequest, EventsRequest, FilesystemRequest, ProjectChildControlAction, RepositoryRequest,
    RunRequest, SessionRequest, TaskRequest, TerminalRequest, UsageRequest, WorkerNodeResources,
    WorkerNodeStatus, WorkspaceRequest,
};
use std::collections::BTreeMap;

fn node(id: u64, is_local: bool) -> WorkerNodeEntry {
    WorkerNodeEntry {
        id,
        status: WorkerNodeStatus {
            node_id: format!("node-{id}"),
            name: format!("Node {id}"),
            online: true,
            capabilities: CapabilitySet::default(),
            resources: WorkerNodeResources {
                cpu_count: 0,
                cpu_usage_percent: None,
                memory_usage_percent: None,
                memory_total_bytes: None,
                memory_available_bytes: None,
                disk_total_bytes: None,
                disk_available_bytes: None,
            },
        },
        is_local,
        url: (!is_local).then(|| format!("ws://worker-{id}/ws")),
        connection: None,
        connection_state: if is_local {
            WorkerConnectionState::Connected
        } else {
            WorkerConnectionState::Disconnected
        },
        connection_detail: None,
        severe_load_streak: 0,
    }
}

#[test]
fn connection_error_details_are_actionable_and_do_not_echo_tokens() {
    let invalid_url = worker_connection_failure_detail(
        WorkerConnectionStage::Transport,
        &LoomError::invalid_request("invalid url with secret-token"),
        Some("secret-token"),
    );
    assert!(invalid_url.contains("Invalid worker URL"));
    assert!(!invalid_url.contains("secret-token"));

    let refused = worker_connection_failure_detail(
        WorkerConnectionStage::Transport,
        &LoomError::new(ErrorCode::Internal, "connection refused", true),
        None,
    );
    assert!(refused.contains("server is running"));

    let timeout = worker_connection_failure_detail(
        WorkerConnectionStage::Transport,
        &LoomError::new(ErrorCode::DeadlineExceeded, "timeout", true),
        None,
    );
    assert!(timeout.contains("timed out"));

    let authentication = worker_connection_failure_detail(
        WorkerConnectionStage::Transport,
        &LoomError::new(ErrorCode::AuthenticationFailed, "denied", false),
        None,
    );
    assert!(authentication.contains("access token"));

    let negotiation = worker_connection_failure_detail(
        WorkerConnectionStage::Negotiation,
        &LoomError::new(ErrorCode::UnsupportedProtocol, "mismatch", false),
        None,
    );
    assert!(negotiation.contains("protocol negotiation failed"));

    let status = worker_connection_failure_detail(
        WorkerConnectionStage::Status,
        &LoomError::new(ErrorCode::Internal, "bad status", false),
        None,
    );
    assert!(status.contains("status request failed"));

    let credential = worker_connection_failure_detail(
        WorkerConnectionStage::CredentialSave,
        &LoomError::new(
            ErrorCode::Persistence,
            "could not persist secret%2Fvalue",
            false,
        ),
        Some("secret/value"),
    );
    assert!(credential.contains("could not save reconnect credentials"));
    assert!(!credential.contains("secret/value"));
    assert!(!credential.contains("secret%2Fvalue"));

    let credential_read = worker_connection_failure_detail(
        WorkerConnectionStage::CredentialRead,
        &LoomError::new(ErrorCode::AuthenticationRequired, "missing", false),
        None,
    );
    assert!(credential_read.contains("OS credential store"));

    let bootstrap_save = worker_connection_failure_detail(
        WorkerConnectionStage::BootstrapSave,
        &LoomError::new(ErrorCode::Persistence, "failed with secret", false),
        Some("secret"),
    );
    assert!(bootstrap_save.contains("browser could not save"));
    assert!(!bootstrap_save.contains("secret"));

    let input = worker_connection_failure_detail(
        WorkerConnectionStage::InputValidation,
        &LoomError::invalid_request("empty input"),
        None,
    );
    assert!(input.contains("URL followed by its access token"));
    let cancelled = worker_connection_failure_detail(
        WorkerConnectionStage::Negotiation,
        &LoomError::new(ErrorCode::RequestCancelled, "closed", false),
        None,
    );
    assert!(cancelled.contains("before negotiation completed"));
    let cancelled_status = worker_connection_failure_detail(
        WorkerConnectionStage::Status,
        &LoomError::new(ErrorCode::RequestCancelled, "closed", false),
        None,
    );
    assert!(cancelled_status.contains("before returning status"));
    let token = worker_connection_failure_detail(
        WorkerConnectionStage::Transport,
        &LoomError::invalid_request("invalid bearer token"),
        None,
    );
    assert!(token.contains("unsupported characters"));
    let timeout_text = worker_connection_failure_detail(
        WorkerConnectionStage::Negotiation,
        &LoomError::new(ErrorCode::Internal, "gateway timeout", false),
        None,
    );
    assert!(timeout_text.contains("timed out"));
    #[cfg(target_family = "wasm")]
    let bootstrap_stage = worker_connection_failure_detail(
        WorkerConnectionStage::Bootstrap,
        &LoomError::new(ErrorCode::Internal, "failed with secret", false),
        Some("secret"),
    );
    #[cfg(target_family = "wasm")]
    {
        assert!(bootstrap_stage.contains("could not open its workspace"));
        assert!(!bootstrap_stage.contains("secret"));
    }
}

#[test]
fn duplicate_connection_attempts_are_blocked_and_url_labels_hide_credentials() {
    let mut state = WorkerConnectionState::Connecting;
    assert!(transition_worker_connection_to_connecting(&mut state, false).is_err());
    assert_eq!(state, WorkerConnectionState::Connecting);

    state = WorkerConnectionState::Failed;
    assert!(transition_worker_connection_to_connecting(&mut state, false).is_ok());
    assert_eq!(state, WorkerConnectionState::Connecting);

    state = WorkerConnectionState::Connected;
    assert!(transition_worker_connection_to_connecting(&mut state, true).is_err());
    assert_eq!(state, WorkerConnectionState::Connected);

    assert!(worker_url_embeds_credential(
        "wss://user:password@worker.example/ws?access_token=sample"
    ));
    assert_eq!(
        safe_worker_url_label(
            "wss://user:password@worker.example/ws?access_token=sample&keep=hidden"
        ),
        "wss://worker.example/ws"
    );
    assert_eq!(
        safe_worker_url_label("user@host/path?secret=value"),
        "host/path"
    );
    assert_eq!(
        safe_worker_url_label("wss://user:pass@worker.example"),
        "wss://worker.example"
    );
    assert_eq!(
        safe_worker_url_label("wss://worker.example/ws#secret"),
        "wss://worker.example/ws"
    );
    assert!(!worker_url_embeds_credential(
        "worker.example/ws?token=hidden"
    ));
    for key in [
        "token",
        "access_token",
        "auth",
        "authorization",
        "bearer",
        "key",
        "api_key",
        "password",
        "secret",
        "client_secret",
    ] {
        assert!(
            worker_url_embeds_credential(&format!("wss://worker.example/ws?{key}=hidden")),
            "{key}"
        );
    }
    assert!(!worker_url_embeds_credential(
        "wss://worker.example/ws?theme=dark"
    ));
}

#[test]
fn worker_node_fixtures_preserve_local_connection_and_hide_url_credentials() {
    let placeholder = connection_placeholder(
        7,
        "wss://user:secret@worker.example/ws?token=hidden".to_owned(),
        WorkerConnectionState::Disconnected,
        Some("offline".to_owned()),
    );
    assert_eq!(placeholder.status.name, "wss://worker.example/ws");
    assert_eq!(
        placeholder.status.node_id,
        "wss://user:secret@worker.example/ws?token=hidden"
    );
    assert!(!placeholder.status.online);
    assert_eq!(placeholder.connection_detail.as_deref(), Some("offline"));

    let local_status = node(ACTIVE_BACKEND_NODE_ENTRY_ID, true).status;
    let local_connection =
        super::ClientConnection::InProcess(Box::new(loom_local::OwnedBackend::new().connect()));
    let config = loom_protocol::WorkspaceConfig {
        worker_nodes: vec![
            loom_protocol::WorkerNodeConfig {
                url: "ws://local-worker/ws".to_owned(),
            },
            loom_protocol::WorkerNodeConfig {
                url: "wss://remote-worker/ws".to_owned(),
            },
        ],
        ..loom_protocol::WorkspaceConfig::default()
    };
    let nodes = initial_worker_nodes(
        local_status,
        local_connection,
        &config,
        Some("ws://local-worker/ws"),
    );
    assert_eq!(nodes.len(), 2);
    assert!(nodes[0].is_local);
    assert_eq!(nodes[0].connection_state, WorkerConnectionState::Connected);
    assert_eq!(nodes[1].id, 1);
    assert_eq!(nodes[1].url.as_deref(), Some("wss://remote-worker/ws"));
    assert_eq!(
        nodes[1].connection_state,
        WorkerConnectionState::Disconnected
    );
    assert!(nodes[1].connection.is_none());
}

#[cfg(not(target_family = "wasm"))]
#[test]
fn failed_connection_state_clears_transport_and_keeps_node_url() {
    let mut node = node(7, false);
    let backend = loom_local::OwnedBackend::new();
    node.connection = Some(super::ClientConnection::InProcess(Box::new(
        backend.connect(),
    )));
    node.status.online = true;
    node.connection_state = WorkerConnectionState::Connected;

    let cleanup_failed =
        mark_worker_connection_failed(&mut node, "protocol negotiation failed".to_owned());

    assert!(!cleanup_failed);
    assert!(node.connection.is_none());
    assert_eq!(node.connection_state, WorkerConnectionState::Failed);
    assert!(!node.status.online);
    assert_eq!(node.url.as_deref(), Some("ws://worker-7/ws"));
    assert_eq!(
        node.connection_detail.as_deref(),
        Some("protocol negotiation failed")
    );
}

fn session(id: AgentSessionId, name: &str) -> AgentSessionSnapshot {
    AgentSessionSnapshot {
        id,
        workspace_id: WorkspaceId::new(),
        name: name.to_owned(),
        state: AgentSessionState::Idle,
        created_at: Timestamp::from_unix_millis(1),
        updated_at: Timestamp::from_unix_millis(1),
    }
}

#[test]
fn session_list_projection_preserves_order_and_selects_active_session() {
    let first = AgentSessionId::new();
    let active = AgentSessionId::new();
    let sessions = vec![session(first, "First"), session(active, "Active")];

    let projection = session_list_projection(&sessions, active);

    assert_eq!(
        projection.entries,
        vec![(first, "First".to_owned()), (active, "Active".to_owned())]
    );
    assert_eq!(projection.selected_index, Some(1));
}

#[test]
fn session_list_projection_has_no_selection_when_active_session_is_missing() {
    let sessions = vec![session(AgentSessionId::new(), "Only session")];

    let projection = session_list_projection(&sessions, AgentSessionId::new());

    assert_eq!(projection.selected_index, None);
}

#[test]
fn project_session_list_projects_nested_children_and_selects_them() {
    let root_id = AgentSessionId::new();
    let child_id = AgentSessionId::new();
    let grandchild_id = AgentSessionId::new();
    let other_id = AgentSessionId::new();
    let sessions = vec![
        session(root_id, "Project"),
        session(child_id, "Researcher"),
        session(grandchild_id, "Analyst"),
        session(other_id, "Other session"),
    ];
    let project_id = loom_core::ProjectId::from_uuid(*root_id.as_uuid());
    let project = loom_core::ProjectSnapshot {
        project_id,
        root_session_id: root_id,
        agents: vec![
            loom_core::ProjectAgentRecord {
                session_id: root_id,
                project_id,
                parent_session_id: None,
                depth: 1,
                state: AgentSessionState::Idle,
                task_summary: None,
                output_cursor: EventSequence::default(),
                updated_at: Timestamp::from_unix_millis(1),
            },
            loom_core::ProjectAgentRecord {
                session_id: child_id,
                project_id,
                parent_session_id: Some(root_id),
                depth: 2,
                state: AgentSessionState::Executing,
                task_summary: Some("Review protocol changes".to_owned()),
                output_cursor: EventSequence::default(),
                updated_at: Timestamp::from_unix_millis(2),
            },
            loom_core::ProjectAgentRecord {
                session_id: grandchild_id,
                project_id,
                parent_session_id: Some(child_id),
                depth: 3,
                state: AgentSessionState::Queued,
                task_summary: Some("Check one detail".to_owned()),
                output_cursor: EventSequence::default(),
                updated_at: Timestamp::from_unix_millis(3),
            },
        ],
        tasks: vec![
            loom_core::DelegatedTaskRecord {
                task_id: loom_core::TaskId::new(),
                project_id,
                requester_session_id: root_id,
                target_session_id: child_id,
                child_name: "Researcher".to_owned(),
                intent: "Review protocol changes".to_owned(),
                model_id: "test-model".to_owned(),
                context_references: Vec::new(),
                dependencies: Vec::new(),
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions::default(),
                status: loom_core::DelegatedTaskStatus::Blocked,
                created_at: Timestamp::from_unix_millis(1),
                updated_at: Timestamp::from_unix_millis(2),
            },
            loom_core::DelegatedTaskRecord {
                task_id: loom_core::TaskId::new(),
                project_id,
                requester_session_id: child_id,
                target_session_id: grandchild_id,
                child_name: "Analyst".to_owned(),
                intent: "Check one detail".to_owned(),
                model_id: "test-model".to_owned(),
                context_references: Vec::new(),
                dependencies: Vec::new(),
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions::default(),
                status: loom_core::DelegatedTaskStatus::Queued,
                created_at: Timestamp::from_unix_millis(2),
                updated_at: Timestamp::from_unix_millis(3),
            },
        ],
        worktrees: vec![],
    };

    assert!(project_snapshot_has_unloaded_agent_sessions(
        &project,
        &sessions[..1]
    ));
    assert!(!project_snapshot_has_unloaded_agent_sessions(
        &project, &sessions
    ));
    let projection = project_session_list_projection(&sessions, child_id, Some(&project));

    assert_eq!(
        projection.entries,
        vec![
            (root_id, "Project".to_owned()),
            (child_id, "Researcher".to_owned()),
            (grandchild_id, "Analyst".to_owned()),
            (other_id, "Other session".to_owned()),
        ]
    );
    assert_eq!(projection.selected_index, Some(1));
    assert_eq!(
        projection.tree[0].children[0].children[0].session_id,
        grandchild_id
    );
    let grandchild_projection =
        project_session_list_projection(&sessions, grandchild_id, Some(&project));
    assert_eq!(grandchild_projection.selected_index, Some(2));

    let other_project_id = loom_core::ProjectId::from_uuid(*other_id.as_uuid());
    let other_project = loom_core::ProjectSnapshot {
        project_id: other_project_id,
        root_session_id: other_id,
        agents: vec![loom_core::ProjectAgentRecord {
            session_id: other_id,
            project_id: other_project_id,
            parent_session_id: None,
            depth: 1,
            state: AgentSessionState::Idle,
            task_summary: None,
            output_cursor: EventSequence::default(),
            updated_at: Timestamp::from_unix_millis(1),
        }],
        tasks: Vec::new(),
        worktrees: Vec::new(),
    };
    let switched_project_projection = project_session_list_projection_for_projects(
        &sessions,
        other_id,
        vec![&project, &other_project],
    );
    assert_eq!(switched_project_projection.selected_index, Some(3));
    assert_eq!(
        switched_project_projection.tree[0].children[0].children[0].session_id,
        grandchild_id
    );
    assert_eq!(switched_project_projection.tree[1].session_id, other_id);
}

#[test]
fn uncovered_session_ids_skip_sessions_captured_by_a_known_project() {
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let other_root = AgentSessionId::new();
    let other_child = AgentSessionId::new();
    let sessions = vec![
        session(root, "Project"),
        session(child, "Agent"),
        session(other_root, "Other project"),
        session(other_child, "Other agent"),
    ];
    let project_id = loom_core::ProjectId::from_uuid(*root.as_uuid());
    let agent = |session_id, parent| loom_core::ProjectAgentRecord {
        session_id,
        project_id,
        parent_session_id: parent,
        depth: 1,
        state: AgentSessionState::Idle,
        task_summary: None,
        output_cursor: EventSequence::default(),
        updated_at: Timestamp::from_unix_millis(1),
    };
    let project = loom_core::ProjectSnapshot {
        project_id,
        root_session_id: root,
        agents: vec![agent(root, None), agent(child, Some(root))],
        tasks: Vec::new(),
        worktrees: Vec::new(),
    };

    // A fetched project covers its root and every delegated agent, so only the
    // still-unknown project's sessions are reported.
    assert_eq!(
        uncovered_session_ids(&sessions, std::slice::from_ref(&project)),
        vec![other_root, other_child]
    );
    assert_eq!(
        uncovered_session_ids(&sessions, &[]),
        vec![root, child, other_root, other_child]
    );
}

#[test]
fn project_order_uses_creation_time_not_recent_activity() {
    let older = AgentSessionId::new();
    let newer = AgentSessionId::new();
    let stamp = Timestamp::from_unix_millis;
    let snapshot = |id, name: &str, created, updated| AgentSessionSnapshot {
        id,
        workspace_id: WorkspaceId::new(),
        name: name.to_owned(),
        state: AgentSessionState::Idle,
        created_at: stamp(created),
        updated_at: stamp(updated),
    };
    // The backend returns sessions by most-recent activity, so the older
    // project arrives first because it was touched last.
    let sessions = vec![
        snapshot(older, "Older", 1, 999),
        snapshot(newer, "Newer", 2, 5),
    ];
    let project = |id: AgentSessionId| {
        let project_id = loom_core::ProjectId::from_uuid(*id.as_uuid());
        loom_core::ProjectSnapshot {
            project_id,
            root_session_id: id,
            agents: vec![loom_core::ProjectAgentRecord {
                session_id: id,
                project_id,
                parent_session_id: None,
                depth: 1,
                state: AgentSessionState::Idle,
                task_summary: None,
                output_cursor: EventSequence::default(),
                updated_at: stamp(1),
            }],
            tasks: Vec::new(),
            worktrees: Vec::new(),
        }
    };
    let (older_project, newer_project) = (project(older), project(newer));

    let projection = project_session_list_projection_for_projects(
        &sessions,
        newer,
        vec![&older_project, &newer_project],
    );

    // Newest project first, regardless of which was updated most recently.
    assert_eq!(projection.tree[0].session_id, newer);
    assert_eq!(projection.tree[1].session_id, older);
}

#[test]
fn project_filter_keeps_matching_projects_whole_and_matches_descendants() {
    let root_id = AgentSessionId::new();
    let child_id = AgentSessionId::new();
    let grandchild_id = AgentSessionId::new();
    let other_id = AgentSessionId::new();
    let tree = vec![
        SessionTreeNode {
            session_id: root_id,
            label: "My API refactor".to_owned(),
            children: vec![SessionTreeNode {
                session_id: child_id,
                label: "Add auth tests".to_owned(),
                children: vec![SessionTreeNode {
                    session_id: grandchild_id,
                    label: "Fix token refresh".to_owned(),
                    children: Vec::new(),
                }],
            }],
        },
        SessionTreeNode {
            session_id: other_id,
            label: "Docs cleanup".to_owned(),
            children: Vec::new(),
        },
    ];
    assert_eq!(session_tree_descendant_count(&tree[0]), 2);
    assert_eq!(session_tree_descendant_count(&tree[1]), 0);

    let root_match = filter_session_tree(tree.clone(), "api");
    assert_eq!(root_match.len(), 1);
    assert_eq!(root_match[0].session_id, root_id);

    // A descendant match keeps the whole project and its path.
    let child_match = filter_session_tree(tree.clone(), "auth");
    assert_eq!(child_match.len(), 1);
    assert_eq!(child_match[0].session_id, root_id);
    assert_eq!(child_match[0].children.len(), 1);
    assert_eq!(child_match[0].children[0].session_id, child_id);

    assert!(filter_session_tree(tree.clone(), "missing").is_empty());

    let other_match = filter_session_tree(tree, "docs");
    assert_eq!(other_match.len(), 1);
    assert_eq!(other_match[0].session_id, other_id);
}

#[test]
fn task_status_pills_cover_every_delegated_task_state() {
    use loom_core::DelegatedTaskStatus as Task;
    for (task, label) in [
        (Task::Queued, "Queued"),
        (Task::Running, "Running"),
        (Task::Blocked, "Blocked"),
        (Task::Completed, "Done"),
        (Task::Failed, "Failed"),
        (Task::Cancelled, "Cancelled"),
    ] {
        assert_eq!(
            session_status_pill(AgentSessionState::Executing, Some(task)).label,
            label
        );
    }
    // A durable task takes precedence over the session state.
    assert_eq!(
        session_status_pill(AgentSessionState::Executing, Some(Task::Blocked)).label,
        "Blocked"
    );
}

#[test]
fn project_child_controls_follow_run_and_task_state() {
    use AgentSessionState as SessionState;
    use ProjectChildControlAction as Action;
    use loom_core::DelegatedTaskStatus as TaskStatus;

    assert_eq!(
        project_child_control_actions(SessionState::Executing, TaskStatus::Running),
        vec![Action::Pause, Action::Interrupt, Action::Cancel]
    );
    assert_eq!(
        project_child_control_actions(SessionState::Paused, TaskStatus::Blocked),
        vec![Action::Continue, Action::Cancel]
    );
    assert_eq!(
        project_child_control_actions(SessionState::Queued, TaskStatus::Queued),
        vec![Action::Continue, Action::Cancel]
    );
    assert_eq!(
        project_child_control_actions(SessionState::Failed, TaskStatus::Failed),
        vec![Action::RetryFailedStep, Action::Cancel]
    );
    assert!(
        project_child_control_actions(SessionState::Completed, TaskStatus::Completed).is_empty()
    );
}

#[test]
fn project_worktree_updates_are_included_in_project_workspace_refreshes() {
    let project_id = loom_core::ProjectId::new();
    let root_session_id = AgentSessionId::new();
    let worktree = loom_core::ProjectWorktreeRecord {
        project_id,
        task_id: loom_core::TaskId::new(),
        parent_session_id: root_session_id,
        child_session_id: AgentSessionId::new(),
        parent_repository_id: loom_core::RepositoryId::new(),
        child_repository_id: loom_core::RepositoryId::new(),
        relative_path: "project-worktrees/example".to_owned(),
        worktree_name: "loom-child-example".to_owned(),
        branch_name: "loom/project-child-example".to_owned(),
        base_revision: "base".to_owned(),
        result_revision: None,
        integrated_revision: None,
        status: loom_core::ProjectWorktreeStatus::Ready,
        conflict_paths: Vec::new(),
        error: None,
        cleanup_disposition: None,
        created_at: loom_core::Timestamp::from_unix_millis(1),
        updated_at: loom_core::Timestamp::from_unix_millis(1),
    };
    let event = loom_protocol::WorkspaceFeedEvent::Session(loom_protocol::ServerEventEnvelope {
        protocol_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
        sequence: EventSequence::new(3),
        session_id: root_session_id,
        event: loom_protocol::ServerEvent::ProjectChildWorktreeUpdated { worktree },
    });
    let members = std::collections::BTreeSet::from([root_session_id]);

    assert!(super::is_project_workspace_event(
        &event, project_id, &members
    ));
    assert!(!super::is_project_workspace_event(
        &event,
        loom_core::ProjectId::new(),
        &members
    ));
}

#[test]
fn source_dialog_initial_choice_tracks_purpose_and_local_availability() {
    assert_eq!(
        source_dialog_initial_state(SessionSourceDialogPurpose::StartSession, true),
        SessionSourceChoice::Empty
    );
    assert_eq!(
        source_dialog_initial_state(SessionSourceDialogPurpose::AddToSession, true),
        SessionSourceChoice::LocalDirectory
    );
    assert_eq!(
        source_dialog_initial_state(SessionSourceDialogPurpose::AddToSession, false),
        SessionSourceChoice::GitHub
    );
}

#[test]
fn local_source_availability_requires_the_local_target() {
    assert!(local_source_available(true, "local", "local"));
    assert!(!local_source_available(false, "local", "local"));
    assert!(!local_source_available(true, "remote", "local"));
    assert!(!local_source_available(false, "remote", "local"));
}

#[test]
fn source_choice_validation_rejects_empty_additions_and_unavailable_local_sources() {
    use SessionSourceChoice::{Empty, GitHub, LocalDirectory};
    use SessionSourceDialogPurpose::{AddToSession, StartSession};

    assert!(source_choice_is_allowed(StartSession, false, Empty));
    assert!(!source_choice_is_allowed(AddToSession, true, Empty));
    assert!(source_choice_is_allowed(AddToSession, true, LocalDirectory));
    assert!(!source_choice_is_allowed(
        AddToSession,
        false,
        LocalDirectory
    ));
    assert!(source_choice_is_allowed(AddToSession, false, GitHub));
}

#[test]
fn local_worker_node_cannot_be_removed() {
    let mut nodes = vec![node(0, true)];

    assert!(remove_worker_node_entry(&mut nodes, 0).is_none());
    assert_eq!(nodes.len(), 1);
    assert!(nodes[0].is_local());
}

#[test]
fn configured_worker_node_can_be_removed_without_removing_local_node() {
    let mut nodes = vec![node(0, true), node(1, false)];

    let removed = remove_worker_node_entry(&mut nodes, 1).unwrap();

    assert_eq!(removed.id, 1);
    assert_eq!(nodes.len(), 1);
    assert!(nodes[0].is_local());
    assert!(remove_worker_node_entry(&mut nodes, 999).is_none());
}

#[test]
fn unavailable_and_available_resource_percentages_are_formatted() {
    assert_eq!(format_percentage(None), "n/a");
    assert_eq!(format_percentage(Some(0)), "0%");
    assert_eq!(format_percentage(Some(73)), "73%");
    assert_eq!(format_percentage(Some(101)), "n/a");
}

#[test]
fn resource_summary_keeps_cpu_cores_and_total_ram_across_samples() {
    let initial = WorkerNodeResources {
        cpu_count: 8,
        cpu_usage_percent: None,
        memory_usage_percent: None,
        memory_total_bytes: Some(16 << 30),
        memory_available_bytes: Some(8 << 30),
        disk_total_bytes: Some(1 << 30),
        disk_available_bytes: Some(512 << 20),
    };
    let initial_summary = format_worker_node_resources(&initial);
    assert!(initial_summary.contains("CPU n/a of 8 cores"));
    assert!(initial_summary.contains("RAM n/a of 16.0 GiB"));
    assert!(initial_summary.contains("disk 512.0 MiB available"));

    let updated = WorkerNodeResources {
        cpu_usage_percent: Some(31),
        memory_usage_percent: Some(50),
        ..initial
    };
    let updated_summary = format_worker_node_resources(&updated);
    assert!(updated_summary.contains("CPU 31% of 8 cores"));
    assert!(updated_summary.contains("RAM 50% of 16.0 GiB"));
    assert!(updated_summary.contains("disk 512.0 MiB available"));
    assert!(
        format_worker_node_resources(&WorkerNodeResources {
            cpu_count: 0,
            cpu_usage_percent: None,
            memory_usage_percent: None,
            memory_total_bytes: None,
            memory_available_bytes: None,
            disk_total_bytes: None,
            disk_available_bytes: None,
        })
        .starts_with("CPU n/a · RAM")
    );
}

#[test]
fn refreshed_worker_status_replaces_initial_unavailable_percentages() {
    let mut nodes = vec![node(1, false)];
    nodes[0].status.resources = WorkerNodeResources {
        cpu_count: 4,
        cpu_usage_percent: None,
        memory_usage_percent: None,
        memory_total_bytes: Some(8 << 30),
        memory_available_bytes: Some(4 << 30),
        disk_total_bytes: Some(100 << 30),
        disk_available_bytes: Some(50 << 30),
    };
    assert!(
        format_worker_node_resources(&nodes[0].status.resources)
            .contains("CPU n/a of 4 cores · RAM n/a of 8.0 GiB")
    );

    let mut refreshed = nodes[0].status.clone();
    refreshed.resources.cpu_usage_percent = Some(25);
    refreshed.resources.memory_usage_percent = Some(50);
    assert_eq!(
        update_worker_node_status(&mut nodes, 1, refreshed.clone()),
        None
    );

    let summary = format_worker_node_resources(&nodes[0].status.resources);
    assert!(summary.contains("CPU 25% of 4 cores · RAM 50% of 8.0 GiB"));
    assert!(summary.contains("disk 50.0 GiB available"));
    assert_eq!(update_worker_node_status(&mut nodes, 999, refreshed), None);
}

#[test]
fn session_snapshot_request_targets_its_session() {
    let session_id = AgentSessionId::new();
    assert_eq!(
        session_id_for_request(
            &ClientRequest::Session(SessionRequest::GetAgentSessionSnapshot { session_id }),
            AgentSessionId::new()
        ),
        Some(session_id)
    );
}

#[test]
fn run_requests_route_to_the_active_session_owner() {
    let active_session_id = AgentSessionId::new();
    let owners = BTreeMap::from([(active_session_id, "peer-node".to_owned())]);
    assert_eq!(
        session_id_for_request(
            &ClientRequest::Run(RunRequest::SendAgentMessage {
                run_id: RunId::new(),
                attempt_id: loom_core::RunAttemptId::new(),
                expected_control_revision: 0,
                message: "hello".to_owned(),
            }),
            active_session_id
        ),
        Some(active_session_id)
    );
    assert_eq!(
        assigned_node_id(&owners, active_session_id),
        Ok("peer-node")
    );
    assert!(assigned_node_id(&BTreeMap::new(), active_session_id).is_err());
    assert_eq!(
        session_id_for_request(
            &ClientRequest::Workspace(WorkspaceRequest::ListWorkspaces),
            active_session_id
        ),
        None
    );
    assert_eq!(
        session_id_for_request(
            &ClientRequest::Filesystem(FilesystemRequest::CreateSessionCheckpoint {
                session_id: active_session_id,
                label: "checkpoint".to_owned(),
            }),
            AgentSessionId::new()
        ),
        Some(active_session_id)
    );
}

#[test]
fn session_request_routing_covers_explicit_active_and_non_session_requests() {
    let session = AgentSessionId::new();
    let active = AgentSessionId::new();
    let repository = loom_core::RepositoryId::new();
    let terminal = loom_core::TerminalId::new();
    let task = loom_core::TaskId::new();
    let checkpoint = loom_core::CheckpointId::new();
    let explicit_requests = vec![
        ClientRequest::Session(SessionRequest::GetAgentSession {
            session_id: session,
        }),
        ClientRequest::Session(SessionRequest::GetAgentSessionSnapshot {
            session_id: session,
        }),
        ClientRequest::Session(SessionRequest::RenameAgentSession {
            session_id: session,
            name: "renamed".into(),
        }),
        ClientRequest::Session(SessionRequest::ArchiveAgentSession {
            session_id: session,
        }),
        ClientRequest::Events(EventsRequest::GetRecentSessionEvents {
            session_id: session,
            limit: 5,
        }),
        ClientRequest::Run(RunRequest::StartSessionAgentRun {
            session_id: session,
            task: "task".into(),
            model: ModelId::new("model"),
            system_instructions: None,
            repository_instructions: None,
        }),
        ClientRequest::Run(RunRequest::StartSessionAgentRunWithOptions {
            session_id: session,
            task: "task".into(),
            model: ModelId::new("model"),
            system_instructions: None,
            repository_instructions: None,
            limits: loom_core::SessionLimits::default(),
            context: loom_protocol::ContextAssemblyOptions::default(),
        }),
        ClientRequest::Repository(RepositoryRequest::AttachSessionRepository {
            session_id: session,
            source: "/repo".into(),
            path: "repo".into(),
            revision: None,
            reuse_local: false,
        }),
        ClientRequest::Filesystem(FilesystemRequest::AttachSessionDirectory {
            session_id: session,
            source: "/folder".into(),
            path: "folder".into(),
        }),
        ClientRequest::Filesystem(FilesystemRequest::ListSessionDirectories {
            session_id: session,
        }),
        ClientRequest::Filesystem(FilesystemRequest::DetachSessionDirectory {
            session_id: session,
            path: "folder".into(),
        }),
        ClientRequest::Repository(RepositoryRequest::ListSessionRepositories {
            session_id: session,
        }),
        ClientRequest::Repository(RepositoryRequest::DetachSessionRepository {
            session_id: session,
            repository_id: repository,
        }),
        ClientRequest::Filesystem(FilesystemRequest::GetSessionFilesystemSnapshot {
            session_id: session,
        }),
        ClientRequest::Filesystem(FilesystemRequest::GetSessionFilesystemChanges {
            session_id: session,
            after_sequence: None,
        }),
        ClientRequest::Filesystem(FilesystemRequest::ReadSessionFile {
            session_id: session,
            path: "file".into(),
        }),
        ClientRequest::Filesystem(FilesystemRequest::ApplySessionFilesystemEdit {
            session_id: session,
            edit: loom_protocol::WorkspaceEdit {
                path: "file".into(),
                old_text: "old".into(),
                new_text: "new".into(),
                expected_revision: None,
            },
        }),
        ClientRequest::Filesystem(FilesystemRequest::TakeSessionFilesystemControl {
            session_id: session,
            control: loom_protocol::WorkspaceControl::Agent,
        }),
        ClientRequest::Filesystem(FilesystemRequest::CreateSessionCheckpoint {
            session_id: session,
            label: "checkpoint".into(),
        }),
        ClientRequest::Filesystem(FilesystemRequest::RevertSessionCheckpoint {
            session_id: session,
            checkpoint_id: checkpoint,
        }),
        ClientRequest::Filesystem(FilesystemRequest::UndoSessionEdit {
            session_id: session,
        }),
        ClientRequest::Filesystem(FilesystemRequest::GetSessionContextFiles {
            session_id: session,
        }),
        ClientRequest::Repository(RepositoryRequest::GetSessionVcsStatus {
            session_id: session,
            repository_id: repository,
        }),
        ClientRequest::Repository(RepositoryRequest::GetSessionVcsDiff {
            session_id: session,
            repository_id: repository,
            path: None,
            staged: false,
        }),
        ClientRequest::Repository(RepositoryRequest::GetSessionVcsBranches {
            session_id: session,
            repository_id: repository,
        }),
        ClientRequest::Repository(RepositoryRequest::GetSessionVcsConflicts {
            session_id: session,
            repository_id: repository,
        }),
        ClientRequest::Terminal(TerminalRequest::OpenSessionTerminal {
            session_id: session,
            command: "sh".into(),
            args: Vec::new(),
            cwd: None,
        }),
        ClientRequest::Terminal(TerminalRequest::WriteSessionTerminalInput {
            session_id: session,
            terminal_id: terminal,
            input: "exit".into(),
        }),
        ClientRequest::Terminal(TerminalRequest::ResizeSessionTerminal {
            session_id: session,
            terminal_id: terminal,
            rows: 24,
            columns: 80,
        }),
        ClientRequest::Terminal(TerminalRequest::GetSessionTerminalEvents {
            session_id: session,
            terminal_id: terminal,
            after_sequence: None,
        }),
        ClientRequest::Terminal(TerminalRequest::CancelSessionTerminal {
            session_id: session,
            terminal_id: terminal,
        }),
        ClientRequest::Task(TaskRequest::StartSessionTask {
            session_id: session,
            spec: loom_protocol::TaskSpec {
                kind: loom_protocol::TaskKind::Test,
                label: "test".into(),
                command: "cargo".into(),
                args: vec!["test".into()],
                cwd: None,
                output_limit_bytes: None,
                artifact_paths: Vec::new(),
            },
        }),
        ClientRequest::Task(TaskRequest::ListSessionTasks {
            session_id: session,
        }),
        ClientRequest::Task(TaskRequest::GetSessionTask {
            session_id: session,
            task_id: task,
        }),
        ClientRequest::Task(TaskRequest::GetSessionTaskEvents {
            session_id: session,
            task_id: task,
            after_sequence: None,
        }),
        ClientRequest::Task(TaskRequest::CancelSessionTask {
            session_id: session,
            task_id: task,
        }),
        ClientRequest::Task(TaskRequest::GetSessionTaskEvidence {
            session_id: session,
            task_id: task,
        }),
        ClientRequest::Session(SessionRequest::SetSessionApprovalPolicy {
            session_id: session,
            policy: loom_core::ApprovalPolicy::default(),
            auto_approve_actions: None,
        }),
        ClientRequest::Session(SessionRequest::ForkAgentSession {
            session_id: session,
            name: "fork".into(),
        }),
        ClientRequest::Usage(UsageRequest::GetSessionUsage {
            session_id: session,
        }),
    ];
    assert!(
        explicit_requests
            .iter()
            .all(|request| { session_id_for_request(request, active) == Some(session) })
    );

    assert_eq!(
        session_id_for_request(
            &ClientRequest::Events(EventsRequest::GetSessionEvents {
                session_id: None,
                workspace_id: None,
                after_sequence: None,
                stream_epoch: None,
            }),
            active
        ),
        Some(active)
    );
    assert_eq!(
        session_id_for_request(
            &ClientRequest::Events(EventsRequest::GetSessionEvents {
                session_id: Some(session),
                workspace_id: None,
                after_sequence: None,
                stream_epoch: None,
            }),
            active
        ),
        Some(session)
    );
    assert_eq!(
        session_id_for_request(
            &ClientRequest::Run(RunRequest::GetAgentRun {
                run_id: RunId::new()
            }),
            active
        ),
        Some(active)
    );
    assert_eq!(
        session_id_for_request(
            &ClientRequest::Usage(UsageRequest::GetRunUsage {
                run_id: RunId::new()
            }),
            active
        ),
        Some(active)
    );
    assert_eq!(
        session_id_for_request(
            &ClientRequest::Workspace(WorkspaceRequest::ListWorkspaces),
            active
        ),
        None
    );
}

#[test]
fn session_aggregation_tracks_node_owners_and_preserves_disconnected_sessions() {
    let primary_id = AgentSessionId::new();
    let peer_id = AgentSessionId::new();
    let disconnected_id = AgentSessionId::new();
    let primary_session = session(primary_id, "Primary");
    let peer_session = session(peer_id, "Peer");
    let disconnected_session = session(disconnected_id, "Disconnected");
    let current = vec![primary_session.clone(), disconnected_session.clone()];
    let owners = BTreeMap::from([
        (primary_id, "node-0".to_owned()),
        (disconnected_id, "removed-node".to_owned()),
    ]);

    let (sessions, owners) = merge_node_sessions(
        &current,
        &owners,
        vec![
            ("node-0".to_owned(), vec![primary_session]),
            ("node-1".to_owned(), vec![peer_session]),
        ],
    );

    assert_eq!(sessions.len(), 3);
    assert_eq!(owners.get(&primary_id).map(String::as_str), Some("node-0"));
    assert_eq!(owners.get(&peer_id).map(String::as_str), Some("node-1"));
    assert_eq!(
        owners.get(&disconnected_id).map(String::as_str),
        Some("removed-node")
    );
}

#[test]
fn session_aggregation_rebinds_returned_sessions_to_a_restarted_node_identity() {
    let session_id = AgentSessionId::new();
    let current = vec![session(session_id, "Existing")];
    let current_owners = BTreeMap::from([(session_id, "old-node-id".to_owned())]);

    let (sessions, owners) = merge_node_sessions(
        &current,
        &current_owners,
        vec![(
            "new-node-id".to_owned(),
            vec![session(session_id, "Existing")],
        )],
    );

    assert_eq!(sessions.len(), 1);
    assert_eq!(
        owners.get(&session_id).map(String::as_str),
        Some("new-node-id")
    );
}

#[test]
fn new_session_node_choices_keep_the_default_backend_first() {
    let nodes = vec![
        ("peer".to_owned(), "External worker".to_owned()),
        ("default".to_owned(), "Local backend".to_owned()),
    ];
    let ordered = order_session_nodes(nodes, "default");

    assert_eq!(ordered[0].0, "default");
    assert_eq!(ordered[1].0, "peer");
}

#[test]
fn model_selection_uses_the_chosen_workers_catalog() {
    let local_model = ModelId::new("local/provider-model");
    let worker_model = ModelId::new("worker/provider-model");
    let catalogs = BTreeMap::from([
        ("local".to_owned(), vec![local_model.clone()]),
        ("worker".to_owned(), vec![worker_model.clone()]),
    ]);

    assert!(validate_model_for_node(&catalogs, "local", &local_model).is_ok());
    assert!(validate_model_for_node(&catalogs, "worker", &local_model).is_err());
    assert!(validate_model_for_node(&catalogs, "worker", &worker_model).is_ok());
    assert_eq!(
        validate_model_for_node(&catalogs, "missing", &worker_model),
        Err("model availability has not been checked".to_owned())
    );
}

#[test]
fn pulse_threshold_adjustment_is_bounded_and_uses_five_percent_by_default() {
    assert_eq!(
        loom_protocol::WorkspaceConfig::default().cpu_pulse_threshold_percent,
        5
    );
    assert_eq!(adjusted_cpu_pulse_threshold(5, -1), 4);
    assert_eq!(adjusted_cpu_pulse_threshold(0, -1), 0);
    assert_eq!(adjusted_cpu_pulse_threshold(100, 1), 100);
    assert_eq!(adjusted_cpu_pulse_threshold(99, 1), 100);
    assert_eq!(adjusted_cpu_pulse_threshold(255, 0), 100);
}

#[test]
fn project_agent_concurrency_adjustment_is_bounded() {
    assert_eq!(
        loom_protocol::WorkspaceConfig::default().project_agent_concurrency,
        4
    );
    assert_eq!(adjusted_project_agent_concurrency(4, -1), 3);
    assert_eq!(
        adjusted_project_agent_concurrency(loom_protocol::MIN_PROJECT_AGENT_CONCURRENCY, -1),
        loom_protocol::MIN_PROJECT_AGENT_CONCURRENCY
    );
    assert_eq!(
        adjusted_project_agent_concurrency(loom_protocol::MAX_PROJECT_AGENT_CONCURRENCY, 1),
        loom_protocol::MAX_PROJECT_AGENT_CONCURRENCY
    );
}

#[test]
fn severe_load_streak_requires_three_consecutive_dual_threshold_samples() {
    let mut status = node(1, false).status;
    status.resources.cpu_usage_percent = Some(91);
    status.resources.memory_usage_percent = Some(91);

    let first = next_severe_load_streak(0, &status.resources);
    let second = next_severe_load_streak(first, &status.resources);
    assert_eq!(second, 2);
    let third = next_severe_load_streak(second, &status.resources);
    assert_eq!(third, 3);
    assert_eq!(next_severe_load_streak(u8::MAX, &status.resources), u8::MAX);

    status.resources.cpu_usage_percent = Some(90);
    assert_eq!(next_severe_load_streak(third, &status.resources), 0);
    status.resources.cpu_usage_percent = Some(91);
    status.resources.memory_usage_percent = None;
    assert_eq!(next_severe_load_streak(third, &status.resources), 0);
}

#[test]
fn active_backend_and_external_worker_labels_do_not_collide() {
    let mut active_backend = node(ACTIVE_BACKEND_NODE_ENTRY_ID, true);
    active_backend.status.name = "local".to_owned();
    let mut external_worker = node(1, false);
    external_worker.status.name = "local".to_owned();

    assert_eq!(
        worker_node_display_name(&active_backend),
        "Local backend · local"
    );
    assert_eq!(
        worker_node_display_name(&external_worker),
        "External worker · local"
    );
}
